//! The one reducer-owned door for durable lane projection.
//!
//! Store operations remain authoritative. This module consumes their exact
//! committed post-state and keeps the rebuildable in-memory worker projection
//! synchronized before ordinary effect dispatch can ask which sessions remain
//! owned.

use super::*;

impl CoreState {
    /// Reset one pending write's lane projection to empty ahead of a
    /// replaceable-operation successor rewrite.
    ///
    /// A member rewritten onto a new generation owns no lane the old
    /// generation minted: nothing may re-attach to them, so the projection
    /// resets to empty exactly as an exact rebuild against an empty recovered
    /// lane set would reset it.
    pub(in crate::core) fn reset_lane_projection_for_successor(&mut self, id: ReceiptId) {
        self.pending.reset_lane_projection(id);
    }

    /// Apply one successful store mutation's exact post-state.
    fn apply_committed_lane(&mut self, lane: &PublishQueueLane) {
        if let Some(id) = self.pending.receipt_for_intent(lane.key.intent_id) {
            self.pending.apply_committed_lane(id, lane);
        }
    }

    fn commit_lane_transition<T>(
        &mut self,
        operation: impl FnOnce(&mut RedbStore) -> Result<(T, PublishQueueLane), PersistenceError>,
    ) -> Result<(T, PublishQueueLane), PersistenceError> {
        let (value, lane) = operation(&mut self.store)?;
        self.apply_committed_lane(&lane);
        Ok((value, lane))
    }

    /// Establish (or re-establish) one intent's projection from the durable
    /// lane set, creating the lanes its recorded route revisions imply.
    ///
    /// A failure leaves the in-memory projection as it was and returns `Err`.
    /// The durable lanes are untouched, so the next boot rebuilds them from
    /// the store: what a failure here costs is progress, never the write.
    pub(in crate::core) fn bootstrap_projected_lanes(
        &mut self,
        intent_id: IntentId,
    ) -> Result<Vec<PublishQueueLane>, PersistenceError> {
        let lanes = self.store.bootstrap_publish_queue_lanes(intent_id)?;
        if let Some(id) = self.pending.receipt_for_intent(intent_id) {
            self.pending.replace_lane_projection(id, &lanes);
        }
        Ok(lanes)
    }

    /// Rebuild one semantic owner's volatile projection from the exact lanes
    /// installed by the atomic current-generation transition.
    ///
    /// Unlike ordinary lane bootstrap, this must not reconcile the current
    /// E2 lane state against retained E1 attempt history. The predecessor
    /// attempts are valid historical evidence, while the current event id is
    /// the fence that decides which physical lanes may run now.
    pub(in crate::core) fn recover_semantic_generation_lanes(
        &mut self,
        intent_id: IntentId,
        event_id: EventId,
    ) -> Result<Vec<PublishQueueLane>, PersistenceError> {
        let lanes = self.store.recover_publish_queue_lanes(intent_id)?;
        if lanes.iter().any(|lane| lane.key.event_id != event_id) {
            return Err(PersistenceError::new(
                "semantic lane recovery found a non-current event generation",
            ));
        }
        if let Some(id) = self.pending.receipt_for_intent(intent_id) {
            self.pending.replace_lane_projection(id, &lanes);
        }
        Ok(lanes)
    }

    pub(in crate::core) fn commit_lane_waiting(
        &mut self,
        key: &PublishQueueLaneKey,
        revision: u64,
        auth: bool,
    ) -> Result<PublishQueueLane, PersistenceError> {
        self.commit_lane_transition(|store| {
            store
                .set_lane_waiting(key, revision, auth)
                .map(|lane| ((), lane))
        })
        .map(|(_, lane)| lane)
    }

    pub(in crate::core) fn commit_lane_eligible(
        &mut self,
        key: &PublishQueueLaneKey,
        revision: u64,
        since: Timestamp,
    ) -> Result<PublishQueueLane, PersistenceError> {
        self.commit_lane_transition(|store| {
            store
                .set_lane_eligible(key, revision, since)
                .map(|lane| ((), lane))
        })
        .map(|(_, lane)| lane)
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::core) fn commit_lane_transient(
        &mut self,
        key: &PublishQueueLaneKey,
        revision: u64,
        ordinal: u64,
        eligible_at: Timestamp,
        cause: PublishQueueTransientCause,
        raw_reason: Option<String>,
    ) -> Result<PublishQueueLane, PersistenceError> {
        self.commit_lane_transition(|store| {
            store
                .set_lane_transient(key, revision, ordinal, eligible_at, cause, raw_reason)
                .map(|lane| ((), lane))
        })
        .map(|(_, lane)| lane)
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::core) fn commit_lane_suspension(
        &mut self,
        key: &PublishQueueLaneKey,
        revision: u64,
        ordinal: u64,
        at: Timestamp,
        cause: PublishQueueTransientCause,
        raw_reason: Option<String>,
        auth: bool,
    ) -> Result<PublishQueueLane, PersistenceError> {
        self.commit_lane_transition(|store| {
            store
                .suspend_lane_attempt(key, revision, ordinal, at, cause, raw_reason, auth)
                .map(|lane| ((), lane))
        })
        .map(|(_, lane)| lane)
    }

    pub(in crate::core) fn commit_lane_attempt_start(
        &mut self,
        key: &PublishQueueLaneKey,
        revision: u64,
        event: SignedEvent,
        started_at: Timestamp,
    ) -> Result<(nmp_store::PublishQueueAttempt, PublishQueueLane), PersistenceError> {
        self.commit_lane_transition(|store| {
            store.start_lane_attempt(key, revision, event, started_at)
        })
    }

    pub(in crate::core) fn commit_lane_handoff(
        &mut self,
        key: &PublishQueueLaneKey,
        revision: u64,
        ordinal: u64,
        detail: PublishQueueAttemptHandoff,
        next: PublishQueuePostHandoffState,
    ) -> Result<PublishQueueLane, PersistenceError> {
        self.commit_lane_transition(|store| {
            store
                .record_lane_handoff(key, revision, ordinal, detail, next)
                .map(|lane| ((), lane))
        })
        .map(|(_, lane)| lane)
    }

    pub(in crate::core) fn commit_lane_attempt_finish(
        &mut self,
        key: &PublishQueueLaneKey,
        revision: u64,
        ordinal: u64,
        outcome: PublishQueueAttemptOutcome,
        finished_at: Timestamp,
    ) -> Result<PublishQueueLane, PersistenceError> {
        self.commit_lane_transition(|store| {
            store
                .finish_lane_attempt(key, revision, ordinal, outcome, finished_at)
                .map(|lane| ((), lane))
        })
        .map(|(_, lane)| lane)
    }

    pub(in crate::core) fn commit_lane_auth_denied(
        &mut self,
        key: &PublishQueueLaneKey,
        revision: u64,
        denial: StoredAuthDenial,
    ) -> Result<PublishQueueLane, PersistenceError> {
        self.commit_lane_transition(|store| {
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
    /// It is nonetheless a door rather than a direct `self.store` call
    /// because under #975 `Auto` re-executes its strategy at every send
    /// opportunity and appends a revision whenever resolution learns
    /// something new — at which point lane minting moves onto this path. The
    /// door plus the enumeration falsifier is what makes that future change
    /// fail mechanically instead of silently projecting nothing.
    pub(in crate::core) fn commit_route_revision(
        &mut self,
        intent_id: IntentId,
        relays: BTreeSet<RelayUrl>,
    ) -> Result<nmp_store::PublishQueueRouteRevision, PersistenceError> {
        self.store.record_route_revision(intent_id, relays)
    }

    /// Close one intent's open work through the projection door.
    ///
    /// The store door validates the all-terminal invariant transactionally,
    /// so the projection contributes no precondition of its own. A failure
    /// changes nothing: the caller keeps the pending write, and with it every
    /// relay the projection still owns, rather than retiring a worker on an
    /// unproven close.
    pub(in crate::core) fn commit_terminal_close(
        &mut self,
        intent_id: IntentId,
    ) -> Result<CloseIntentOutcome, PersistenceError> {
        self.store.close_terminal_intent(intent_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nmp_store::RedbStore;
    use nostr::{Keys, Kind};
    use std::time::Instant;

    /// Accept and sign one durable private write, which routes and bootstraps
    /// its lanes.
    fn publish_to(
        core: &mut CoreState,
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

    fn publish_waiting(
        core: &mut CoreState,
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
                    Some(author.public_key())
                )
        )));
        (id, signed)
    }

    /// Independent semantic oracle: reconstruct the old exact answer from
    /// canonical store rows plus the reducer's non-lane transient owners.
    /// This deliberately does not inspect `LaneWorkerProjection`.
    fn durable_worker_oracle(core: &CoreState) -> BTreeSet<RelaySessionKey> {
        let mut expected: BTreeSet<_> = core
            .attempt_correlations
            .values()
            .map(|target| target.session.clone())
            .collect();
        for pending in core.pending.values() {
            let access = Some(pending.signing_pubkey);
            expected.extend(
                pending
                    .pending_relays
                    .iter()
                    .cloned()
                    .map(|relay| RelaySessionKey::new(relay, access)),
            );
            expected.extend(
                core.store
                    .recover_publish_queue_lanes(pending.intent_id)
                    .expect("oracle lane recovery")
                    .into_iter()
                    .filter(|lane| !matches!(lane.state, PublishQueueLaneState::Terminal { .. }))
                    .map(|lane| RelaySessionKey::new(lane.key.relay, access)),
            );
        }
        expected
    }

    fn assert_projection_matches_store(core: &CoreState) {
        let actual = core.relay_worker_requirements().writes;
        assert_eq!(actual, durable_worker_oracle(core));
    }

    #[test]
    fn projection_matches_durable_state_after_every_normal_publish_queue_transition() {
        let author = Keys::generate();
        let relay = RelayUrl::parse("wss://projection-lifecycle.example.com").unwrap();
        let session =
            RelaySessionKey::new(relay.clone(), Some(author.public_key()));
        let mut core = CoreState::new(RedbStore::temporary().expect("temporary Redb store"), 10);

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
            !core.pending.contains(&receipt),
            "the exact terminal projection allows the store-validated close"
        );
    }

    #[test]
    fn same_url_keeps_distinct_signing_identities_in_worker_demand() {
        let author_a = Keys::generate();
        let author_b = Keys::generate();
        let relay = RelayUrl::parse("wss://projection-identity.example.com").unwrap();
        let mut core = CoreState::new(RedbStore::temporary().expect("temporary Redb store"), 10);

        publish_waiting(&mut core, &author_a, &relay, 10);
        publish_waiting(&mut core, &author_b, &relay, 11);
        assert_projection_matches_store(&core);

        let actual = core.relay_worker_requirements().writes;
        assert_eq!(
            actual,
            BTreeSet::from([
                RelaySessionKey::new(relay.clone(), Some(author_a.public_key())),
                RelaySessionKey::new(relay.clone(), Some(author_b.public_key())),
            ])
        );
        assert!(!actual.contains(&RelaySessionKey::unauthenticated(relay)));
    }

    #[test]
    fn close_reopen_rebuilds_the_same_exact_worker_projection() {
        let author_a = Keys::generate();
        let author_b = Keys::generate();
        let relay = RelayUrl::parse("wss://projection-restart.example.com").unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("worker-projection.redb");
        let expected = BTreeSet::from([
            RelaySessionKey::new(relay.clone(), Some(author_a.public_key())),
            RelaySessionKey::new(relay.clone(), Some(author_b.public_key())),
        ]);

        {
            let mut core = CoreState::new(RedbStore::open(&path).unwrap(), 10);
            publish_waiting(&mut core, &author_a, &relay, 20);
            publish_waiting(&mut core, &author_b, &relay, 21);
            assert_projection_matches_store(&core);
            assert_eq!(core.relay_worker_requirements().writes, expected);
        }

        let mut recovered = CoreState::new(RedbStore::open(&path).unwrap(), 10);
        let effects = recovered.recover_on_boot();
        assert_projection_matches_store(&recovered);
        assert_eq!(
            recovered.relay_worker_requirements().writes,
            expected
        );
        for session in expected {
            assert!(effects.iter().any(
                |effect| matches!(effect, Effect::EnsureWriteRelay(candidate) if candidate == &session)
            ));
        }
    }

    /// A lane transition that does not commit leaves the projection exactly
    /// as it was. There is no third state between "committed" and "did not":
    /// the store is authoritative, and a failure is simply progress this pass
    /// did not make.
    #[test]
    fn a_failed_lane_transition_leaves_the_exact_projection_unchanged() {
        let author = Keys::generate();
        let relay = RelayUrl::parse("wss://projection-absent.example.com").unwrap();
        let mut core = CoreState::new(RedbStore::temporary().expect("temporary Redb store"), 10);
        let (receipt, _) = publish_waiting(&mut core, &author, &relay, 31);
        let before = core.pending[&receipt].lane_projection.clone();

        let result: Result<((), PublishQueueLane), PersistenceError> =
            core.commit_lane_transition(|_store| {
                Err(PersistenceError::new("injected failed transition"))
            });

        result.expect_err("the injected transition must refuse");
        assert_eq!(core.pending[&receipt].lane_projection, before);
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
    /// 1. **Nothing is missing from the enumeration.** Every concrete
    ///    `RedbStore` door that takes `&mut self` and deals in
    ///    `PublishQueueLane` is a lane-mutation constructor, scraped straight
    ///    out of `nmp-store`'s inherent implementation; adding one there
    ///    without giving it a `commit_*` door in this module fails here. Two
    ///    further constructors never mention `PublishQueueLane` in their
    ///    signature and are therefore named explicitly:
    ///    `close_terminal_intent`, which removes an intent's open work
    ///    wholesale, and `record_route_revision`.
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
    ///    lane-mutation door. Matching is on `.method(` rather than on
    ///    `self.store.method(`, so binding the store to a local first does not
    ///    evade it. Comments are removed and whitespace stripped before
    ///    matching, so rustfmt's multi-line
    ///    `self\n.store\n.set_lane_waiting(` chain is caught exactly like the
    ///    single-line spelling, while a commented-out call neither fails the
    ///    build spuriously nor hides a real one behind a trailing comment.
    #[test]
    fn every_lane_mutation_constructor_goes_through_the_projection_door() {
        /// Lane-minting/removing doors whose signature does not mention
        /// `PublishQueueLane`, so the scrape below cannot find them.
        const NAMED_EXPLICITLY: &[&str] = &["record_route_revision", "close_terminal_intent"];

        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let store_source =
            std::fs::read_to_string(manifest.join("../nmp-store/src/redb_store/mod.rs"))
                .expect("read nmp-store/src/redb_store/mod.rs");

        let mut doors: Vec<String> = Vec::new();
        for chunk in store_source.split("\n    pub fn ").skip(1) {
            let name = chunk.split('(').next().unwrap_or_default();
            let signature = chunk.split('{').next().unwrap_or_default();
            if signature.contains("&mut self") && signature.contains("PublishQueueLane") {
                doors.push(name.to_string());
            }
        }
        assert!(
            doors.len() >= 8,
            "the signature scrape found only {} lane-mutation doors, which means \
             the scrape itself broke rather than the concrete delivery door \
             shrinking: {doors:?}",
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
                "`RedbStore::{door}` mutates lane state but core/lane_projection.rs \
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
                for door in &doors {
                    if squeezed.contains(&format!("store_mut().{door}(")) {
                        offenders.push(format!("{} calls store_mut().{door}", path.display()));
                    } else if squeezed.contains(&format!(".{door}(")) {
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
    /// [`CoreState::write_relay_workers`] (the projection) versus
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
        let mut core = CoreState::new(RedbStore::open(&path).unwrap(), INTENTS + 1);

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
