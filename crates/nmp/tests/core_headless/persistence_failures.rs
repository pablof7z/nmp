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
pub(super) struct FailIngestStore {
    inner: MemoryStore,
    fail_insert: bool,
    fail_coverage: bool,
    fail_query: Rc<Cell<bool>>,
    coverage_batch_sizes: Rc<RefCell<Vec<usize>>>,
}

impl FailIngestStore {
    pub(super) fn armed() -> Self {
        Self {
            inner: MemoryStore::new(),
            fail_insert: true,
            fail_coverage: false,
            fail_query: Rc::new(Cell::new(false)),
            coverage_batch_sizes: Rc::new(RefCell::new(Vec::new())),
        }
    }

    fn coverage_armed(coverage_batch_sizes: Rc<RefCell<Vec<usize>>>) -> Self {
        Self {
            inner: MemoryStore::new(),
            fail_insert: false,
            fail_coverage: true,
            fail_query: Rc::new(Cell::new(false)),
            coverage_batch_sizes,
        }
    }

    fn projection_armed(fail_query: Rc<Cell<bool>>) -> Self {
        Self {
            inner: MemoryStore::new(),
            fail_insert: false,
            fail_coverage: false,
            fail_query,
            coverage_batch_sizes: Rc::new(RefCell::new(Vec::new())),
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
            self.fail_insert = false;
            // Classified as the real backend would classify a disk-full
            // write (#895): the originating I/O failure, durability unknown
            // — never `invariant`, which would claim the write is absent.
            return Err(PersistenceError::new(
                PersistenceFault::Io,
                "injected ingest I/O failure",
            ));
        }
        self.inner.insert(event, from)
    }
    fn query(&self, filter: &nostr::Filter) -> Result<Vec<StoredEvent>, PersistenceError> {
        if self.fail_query.replace(false) {
            return Err(PersistenceError::invariant(
                "injected post-commit projection read failure",
            ));
        }
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
        claims: &[(nmp_grammar::ContextualAtom, RelayUrl, CoverageInterval)],
    ) -> Result<(), PersistenceError> {
        self.coverage_batch_sizes.borrow_mut().push(claims.len());
        if self.fail_coverage {
            self.fail_coverage = false;
            return Err(PersistenceError::new(
                PersistenceFault::Io,
                "injected request-level coverage failure",
            ));
        }
        self.inner.record_coverage(claims)
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
    fn recover_outbox(&self) -> Result<Vec<RecoveredIntent>, PersistenceError> {
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
///
/// It also pins #895's classification across the crate boundary: the fault
/// and its durability outcome reach `nmp` as types, so a consumer
/// never has to read the message to learn whether the write may have landed.
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
    let error = outcome.expect_err("an ingest-path I/O failure must surface as Err");
    assert_eq!(error.fault(), PersistenceFault::Io);
    assert_eq!(
        error.durability(),
        DurabilityOutcome::Unknown,
        "an I/O failure never claims the write is absent"
    );
    assert!(error.fault().requires_reopen());
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

    let _ = core.handle(EngineMsg::Subscribe(literal_query(
        &[1],
        &a.public_key().to_hex(),
    )));
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

/// #816's ordinary-REQ failure falsifier: an EVENT that fails its durable
/// commit poisons only the exact request revision named by that frame. Its
/// later EOSE therefore cannot manufacture a coverage fact for data the
/// store never accepted.
#[test]
fn failed_event_commit_prevents_its_exact_request_from_recording_coverage() {
    let author = Keys::generate();
    let healthy_author = Keys::generate();
    let relay = RelayUrl::parse("wss://relay.example.com").unwrap();
    let healthy_relay = RelayUrl::parse("wss://healthy-relay.example.com").unwrap();
    let dir = FixtureDirectory::new()
        .with_write(author.public_key().to_hex(), [relay.clone()])
        .with_write(
            healthy_author.public_key().to_hex(),
            [healthy_relay.clone()],
        );
    let mut core = EngineCore::new(FailIngestStore::armed(), Box::new(dir), 10);

    let _ = core.handle(EngineMsg::Subscribe(literal_query(
        &[1],
        &author.public_key().to_hex(),
    )));
    let _ = core.handle(EngineMsg::Subscribe(literal_query(
        &[2],
        &healthy_author.public_key().to_hex(),
    )));
    let connected = connect(&mut core, 0, &relay);
    let request = connected
        .iter()
        .find_map(|effect| match effect {
            Effect::Replay(session, requests) if session == &public_session(&relay) => requests
                .iter()
                .find(|request| {
                    request
                        .filter
                        .kinds
                        .as_ref()
                        .is_some_and(|kinds| kinds.contains(&1))
                })
                .map(|request| request.sub_id.clone()),
            _ => None,
        })
        .expect("connect replays the exact kind:1 request");
    let healthy_connected = connect(&mut core, 1, &healthy_relay);
    let healthy_request = healthy_connected
        .iter()
        .find_map(|effect| match effect {
            Effect::Replay(session, requests) if session == &public_session(&healthy_relay) => {
                requests
                    .iter()
                    .find(|request| {
                        request
                            .filter
                            .kinds
                            .as_ref()
                            .is_some_and(|kinds| kinds.contains(&2))
                    })
                    .map(|request| request.sub_id.clone())
            }
            _ => None,
        })
        .expect("healthy connect replays its independent kind:2 request");
    let wire = wire_sub_string(&request);
    let _ = core.handle(EngineMsg::Tick(Timestamp::from(500u64)));

    let failed = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay),
        event_frame(
            &wire,
            nmp_resolver::testkit::kind1(&author, "must not earn coverage", 100),
        ),
    ));
    assert!(failed
        .iter()
        .any(|effect| matches!(effect, Effect::EmitDiagnostics(snapshot)
            if snapshot.store_degraded.is_some())));

    let completed = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay),
        eose_frame(&wire),
    ));
    assert!(
        !completed
            .iter()
            .any(|effect| matches!(effect, Effect::RecordCoverage(..))),
        "the poisoned request must retire without a coverage effect: {completed:?}"
    );
    let atom = ctx_atom(cf(&[1], &[&author.public_key().to_hex()]));
    assert_eq!(core.get_coverage(&atom, &relay), None);

    let healthy_wire = wire_sub_string(&healthy_request);
    let healthy_event = nostr::EventBuilder::new(Kind::Custom(2), "healthy")
        .custom_created_at(Timestamp::from(101u64))
        .sign_with_keys(&healthy_author)
        .expect("fixture signing");
    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 1,
            generation: 1,
        },
        public_session(&healthy_relay),
        event_frame(&healthy_wire, healthy_event),
    ));
    let healthy_completed = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 1,
            generation: 1,
        },
        public_session(&healthy_relay),
        eose_frame(&healthy_wire),
    ));
    assert!(
        healthy_completed
            .iter()
            .any(|effect| matches!(effect, Effect::RecordCoverage(..))),
        "another in-flight request remains eligible after the first fails"
    );
    let healthy_atom = ctx_atom(cf(&[2], &[&healthy_author.public_key().to_hex()]));
    assert!(core.get_coverage(&healthy_atom, &healthy_relay).is_some());
}

/// The event-failure key is the full physical relay session, not the relay
/// URL. Two identical pinned selections therefore keep independent coverage
/// authority when one runs on Public and the other on `Nip42(author)`.
#[test]
fn failed_event_commit_isolated_by_access_context_on_the_same_relay() {
    let public_author = Keys::generate();
    let protected_author = Keys::generate();
    let relay = RelayUrl::parse("wss://access-isolation.example.com").unwrap();
    let source = SourceAuthority::Pinned(BTreeSet::from([relay.clone()]));
    let selection = Filter {
        kinds: Some(BTreeSet::from([1u16])),
        ..Filter::default()
    };
    let public_query = LiveQuery(
        nmp_grammar::Demand::new(selection.clone(), source.clone(), AccessContext::Public)
            .expect("public pinned demand"),
    );
    let protected_query = LiveQuery(
        nmp_grammar::Demand::new(
            selection,
            source.clone(),
            AccessContext::Nip42(protected_author.public_key()),
        )
        .expect("protected pinned demand"),
    );
    let mut core = EngineCore::new(
        FailIngestStore::armed(),
        Box::new(FixtureDirectory::new()),
        10,
    );

    let _ = core.handle(EngineMsg::Subscribe(public_query));
    let _ = core.handle(EngineMsg::Subscribe(protected_query));

    let public_connected = connect(&mut core, 0, &relay);
    let public_request = public_connected
        .iter()
        .find_map(|effect| match effect {
            Effect::Replay(session, requests) if session == &public_session(&relay) => {
                requests.first().map(|request| request.sub_id.clone())
            }
            _ => None,
        })
        .expect("public session replays its pinned request");

    let protected_session = signer_session(&relay, protected_author.public_key());
    let protected_connected = connect_signer(&mut core, 1, &relay, protected_author.public_key());
    assert_no_protected_req(&protected_connected, &protected_session);
    let protected_ready = authenticate_signer(&mut core, 1, &relay, &protected_author);
    let protected_request = protected_ready
        .iter()
        .find_map(|effect| match effect {
            Effect::Replay(session, requests) if session == &protected_session => {
                requests.first().map(|request| request.sub_id.clone())
            }
            _ => None,
        })
        .expect("authenticated session replays its identical pinned request");

    let public_filter = public_connected
        .iter()
        .find_map(|effect| match effect {
            Effect::Replay(session, requests) if session == &public_session(&relay) => {
                requests.first().map(|request| request.filter.clone())
            }
            _ => None,
        })
        .expect("public replay carries its filter");
    let protected_filter = protected_ready
        .iter()
        .find_map(|effect| match effect {
            Effect::Replay(session, requests) if session == &protected_session => {
                requests.first().map(|request| request.filter.clone())
            }
            _ => None,
        })
        .expect("protected replay carries its filter");
    assert_eq!(
        public_filter, protected_filter,
        "selection and source are identical; access context is the only partition"
    );

    let _ = core.handle(EngineMsg::Tick(Timestamp::from(500u64)));
    let failed = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay),
        event_frame(
            &wire_sub_string(&public_request),
            nmp_resolver::testkit::kind1(&public_author, "the public transaction fails", 100),
        ),
    ));
    assert!(failed
        .iter()
        .any(|effect| matches!(effect, Effect::EmitDiagnostics(snapshot)
            if snapshot.store_degraded.is_some())));

    let protected_event =
        nmp_resolver::testkit::kind1(&protected_author, "the protected transaction commits", 101);
    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 1,
            generation: 1,
        },
        protected_session.clone(),
        event_frame(&wire_sub_string(&protected_request), protected_event),
    ));

    let public_completed = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay),
        eose_frame(&wire_sub_string(&public_request)),
    ));
    assert!(
        !public_completed
            .iter()
            .any(|effect| matches!(effect, Effect::RecordCoverage(..))),
        "the failed Public request must remain poisoned"
    );
    let protected_completed = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 1,
            generation: 1,
        },
        protected_session,
        eose_frame(&wire_sub_string(&protected_request)),
    ));
    assert!(
        protected_completed
            .iter()
            .any(|effect| matches!(effect, Effect::RecordCoverage(..))),
        "the identical request on another access session remains eligible"
    );

    let concrete = ConcreteFilter {
        kinds: Some(BTreeSet::from([1u16])),
        ..ConcreteFilter::default()
    };
    let public_atom = ContextualAtom {
        filter: concrete.clone(),
        source: source.clone(),
        access: AccessContext::Public,
        routing_evidence: BTreeSet::new(),
    };
    let protected_atom = ContextualAtom {
        filter: concrete,
        source,
        access: AccessContext::Nip42(protected_author.public_key()),
        routing_evidence: BTreeSet::new(),
    };
    assert_eq!(core.get_coverage(&public_atom, &relay), None);
    assert!(core.get_coverage(&protected_atom, &relay).is_some());
}

/// An EVENT on an overwritten wire id cannot identify which outstanding REQ
/// revision emitted it, so every owner already in that exact FIFO is
/// poisoned. A revision sent after the failure is not retroactively poisoned.
#[test]
fn failed_event_commit_poisons_only_the_then_current_wire_fifo_revisions() {
    let a = Keys::generate();
    let b = Keys::generate();
    let c = Keys::generate();
    let relay = RelayUrl::parse("wss://fifo-isolation.example.com").unwrap();
    let dir = FixtureDirectory::new()
        .with_write(a.public_key().to_hex(), [relay.clone()])
        .with_write(b.public_key().to_hex(), [relay.clone()])
        .with_write(c.public_key().to_hex(), [relay.clone()]);
    let mut core = EngineCore::new(FailIngestStore::armed(), Box::new(dir), 10);
    connect(&mut core, 0, &relay);

    let first = core.handle(EngineMsg::Subscribe(literal_query(
        &[1],
        &a.public_key().to_hex(),
    )));
    let first_sub = req_for(&first, &relay).0.clone();
    let second = core.handle(EngineMsg::Subscribe(literal_query(
        &[1],
        &b.public_key().to_hex(),
    )));
    let second_sub = req_for(&second, &relay).0.clone();
    assert_eq!(
        first_sub, second_sub,
        "the filter widening reuses one wire FIFO"
    );
    let wire = wire_sub_string(&first_sub);

    let _ = core.handle(EngineMsg::Tick(Timestamp::from(500u64)));
    let failed = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay),
        event_frame(
            &wire,
            nmp_resolver::testkit::kind1(&a, "ambiguous failed revision", 100),
        ),
    ));
    assert!(failed
        .iter()
        .any(|effect| matches!(effect, Effect::EmitDiagnostics(snapshot)
            if snapshot.store_degraded.is_some())));

    let third = core.handle(EngineMsg::Subscribe(literal_query(
        &[1],
        &c.public_key().to_hex(),
    )));
    let third_sub = req_for(&third, &relay).0.clone();
    assert_eq!(
        first_sub, third_sub,
        "the post-failure widening is a later revision of the same wire FIFO"
    );

    let atom_a = ctx_atom(cf(&[1], &[&a.public_key().to_hex()]));
    let atom_b = ctx_atom(cf(&[1], &[&b.public_key().to_hex()]));
    let atom_c = ctx_atom(cf(&[1], &[&c.public_key().to_hex()]));
    for now in [600u64, 700] {
        let _ = core.handle(EngineMsg::Tick(Timestamp::from(now)));
        let poisoned = core.handle(EngineMsg::RelayFrame(
            RelayHandle {
                slot: 0,
                generation: 1,
            },
            public_session(&relay),
            eose_frame(&wire),
        ));
        assert!(
            !poisoned
                .iter()
                .any(|effect| matches!(effect, Effect::RecordCoverage(..))),
            "both revisions present at the failed EVENT stay poisoned: {poisoned:?}"
        );
        assert_eq!(core.get_coverage(&atom_a, &relay), None);
        assert_eq!(core.get_coverage(&atom_b, &relay), None);
        assert_eq!(core.get_coverage(&atom_c, &relay), None);
    }

    let _ = core.handle(EngineMsg::Tick(Timestamp::from(800u64)));
    let later = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay),
        eose_frame(&wire),
    ));
    assert!(
        later
            .iter()
            .any(|effect| matches!(effect, Effect::RecordCoverage(..))),
        "the revision recorded after the failure remains eligible: {later:?}"
    );
    assert!(core.get_coverage(&atom_a, &relay).is_some());
    assert!(core.get_coverage(&atom_b, &relay).is_some());
    assert!(core.get_coverage(&atom_c, &relay).is_some());
}

/// A projection read can fail only after the EVENT transaction has committed.
/// That phase still degrades diagnostics, but it must not revoke the exact
/// request's coverage authority: doing so would confuse a failed local view
/// refresh with a missing durable fact.
#[test]
fn post_commit_projection_failure_does_not_poison_request_coverage() {
    let author = Keys::generate();
    let followed = Keys::generate();
    let relay = RelayUrl::parse("wss://projection-failure.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(author.public_key().to_hex(), [relay.clone()]);
    let fail_query = Rc::new(Cell::new(false));
    let store = FailIngestStore::projection_armed(fail_query.clone());
    let mut core = EngineCore::new(store, Box::new(dir), 10);
    let _ = core.handle(EngineMsg::SetActivePubkey(Some(author.public_key())));
    let my_follows = LiveQuery::from_filter(Filter {
        kinds: Some(BTreeSet::from([1u16])),
        authors: Some(Binding::Derived(Box::new(nmp_grammar::Derived {
            inner: nmp_grammar::Demand::from_filter(Filter {
                kinds: Some(BTreeSet::from([3u16])),
                authors: Some(Binding::Reactive(nmp_grammar::IdentityField::ActivePubkey)),
                ..Filter::default()
            }),
            project: nmp_grammar::Selector::Tag("p".to_string()),
        }))),
        ..Filter::default()
    });
    let _ = core.handle(EngineMsg::Subscribe(my_follows));
    let connected = connect(&mut core, 0, &relay);
    let request = connected
        .iter()
        .find_map(|effect| match effect {
            Effect::Replay(session, requests) if session == &public_session(&relay) => requests
                .iter()
                .find(|request| {
                    request
                        .filter
                        .kinds
                        .as_ref()
                        .is_some_and(|kinds| kinds.contains(&3))
                })
                .map(|request| request.sub_id.clone()),
            _ => None,
        })
        .expect("connect replays the derived query's kind:3 request");
    let wire = wire_sub_string(&request);
    let _ = core.handle(EngineMsg::Tick(Timestamp::from(500u64)));

    fail_query.set(true);
    let event = nmp_resolver::testkit::kind3(&author, &[followed.public_key()], 100);
    let failed_projection = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay),
        event_frame(&wire, event),
    ));
    assert!(failed_projection
        .iter()
        .any(|effect| matches!(effect, Effect::EmitDiagnostics(snapshot)
            if snapshot.store_degraded.is_some())));

    let completed = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay),
        eose_frame(&wire),
    ));
    assert!(
        completed
            .iter()
            .any(|effect| matches!(effect, Effect::RecordCoverage(..))),
        "the committed event's request remains eligible despite projection failure"
    );
    let atom = ctx_atom(cf(&[3], &[&author.public_key().to_hex()]));
    assert!(core.get_coverage(&atom, &relay).is_some());
}

/// #816's request-atomic coverage falsifier. Two narrow atoms coalesced into
/// one wire request cross the store boundary as one batch; an injected
/// failure leaves neither claim visible and emits no success effect. A
/// separate request that was already in flight remains eligible and commits
/// normally afterward, proving the failure is not a process-wide latch.
#[test]
fn coverage_failure_is_atomic_for_one_request_and_isolated_from_another() {
    let a = Keys::generate();
    let b = Keys::generate();
    let healthy = Keys::generate();
    let failed_relay = RelayUrl::parse("wss://failed-coverage.example.com").unwrap();
    let healthy_relay = RelayUrl::parse("wss://healthy-coverage.example.com").unwrap();
    let dir = FixtureDirectory::new()
        .with_write(a.public_key().to_hex(), [failed_relay.clone()])
        .with_write(b.public_key().to_hex(), [failed_relay.clone()])
        .with_write(healthy.public_key().to_hex(), [healthy_relay.clone()]);
    let batch_sizes = Rc::new(RefCell::new(Vec::new()));
    let store = FailIngestStore::coverage_armed(batch_sizes.clone());
    let mut core = EngineCore::new(store, Box::new(dir), 10);

    for author in [&a, &b, &healthy] {
        let _ = core.handle(EngineMsg::Subscribe(literal_query(
            &[1],
            &author.public_key().to_hex(),
        )));
    }
    let failed_connect = connect(&mut core, 0, &failed_relay);
    let failed_request = failed_connect
        .iter()
        .find_map(|effect| match effect {
            Effect::Replay(session, requests) if session == &public_session(&failed_relay) => {
                requests
                    .iter()
                    .find(|request| {
                        request
                            .filter
                            .kinds
                            .as_ref()
                            .is_some_and(|kinds| kinds.contains(&1))
                    })
                    .cloned()
            }
            _ => None,
        })
        .expect("failed relay replays its coalesced request");
    assert_eq!(
        failed_request.absorbed.len(),
        2,
        "the one request must carry both narrow coverage atoms"
    );

    let healthy_connect = connect(&mut core, 1, &healthy_relay);
    let healthy_request = healthy_connect
        .iter()
        .find_map(|effect| match effect {
            Effect::Replay(session, requests) if session == &public_session(&healthy_relay) => {
                requests
                    .iter()
                    .find(|request| {
                        request
                            .filter
                            .kinds
                            .as_ref()
                            .is_some_and(|kinds| kinds.contains(&1))
                    })
                    .map(|request| request.sub_id.clone())
            }
            _ => None,
        })
        .expect("healthy relay replays its independent request");
    let _ = core.handle(EngineMsg::Tick(Timestamp::from(500u64)));

    let failed = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&failed_relay),
        eose_frame(&wire_sub_string(&failed_request.sub_id)),
    ));
    assert!(
        !failed
            .iter()
            .any(|effect| matches!(effect, Effect::RecordCoverage(..))),
        "a failed request-level batch emits no per-claim success effect: {failed:?}"
    );
    let atom_a = ctx_atom(cf(&[1], &[&a.public_key().to_hex()]));
    let atom_b = ctx_atom(cf(&[1], &[&b.public_key().to_hex()]));
    assert_eq!(core.get_coverage(&atom_a, &failed_relay), None);
    assert_eq!(core.get_coverage(&atom_b, &failed_relay), None);

    let succeeded = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 1,
            generation: 1,
        },
        public_session(&healthy_relay),
        eose_frame(&wire_sub_string(&healthy_request)),
    ));
    assert!(succeeded
        .iter()
        .any(|effect| matches!(effect, Effect::RecordCoverage(..))));
    let healthy_atom = ctx_atom(cf(&[1], &[&healthy.public_key().to_hex()]));
    assert!(core.get_coverage(&healthy_atom, &healthy_relay).is_some());
    assert_eq!(
        batch_sizes.borrow().as_slice(),
        &[2, 1],
        "one store call per completed request, never one call per atom"
    );
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
        claims: &[(nmp_grammar::ContextualAtom, RelayUrl, CoverageInterval)],
    ) -> Result<(), PersistenceError> {
        self.inner.record_coverage(claims)
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
    fn recover_outbox(&self) -> Result<Vec<RecoveredIntent>, PersistenceError> {
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
            return Err(PersistenceError::invariant(
                "injected bootstrap failure".to_string(),
            ));
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
        let accepted = core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::Event(draft(100 + i as u64, &format!("falsifier {i}"))),
            durability: Durability::Durable,
            routing: WriteRouting::Explicit(vec![relay.clone()]),
            identity: Identity::Active,
            correlation: None,
        }));
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

/// BDD falsifier for #985: answering whether an unchanged durable write still
/// owns its relay worker is reducer state, not recovery. `RelayOpenFailed`
/// asks that question once to decide whether the failure is still relevant
/// and pruning asks it again after recording the fact. Neither answer may
/// range-scan or decode the durable lane table after signing/bootstrap has
/// already established the exact lane state.
#[test]
fn unchanged_worker_demand_reads_zero_outbox_lanes() {
    const N: usize = 3;
    let author = Keys::generate();
    let relays: Vec<RelayUrl> = (0..N)
        .map(|i| RelayUrl::parse(&format!("wss://worker-projection-{i}.example.com")).unwrap())
        .collect();

    let calls = Rc::new(Cell::new(0u64));
    let mut core = EngineCore::new(
        WakeLaneProbeStore::new(calls.clone()),
        Box::new(FixtureDirectory::new()),
        10,
    );
    activate(&mut core, &author);

    for (i, relay) in relays.iter().enumerate() {
        let accepted = core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::Event(draft(300 + i as u64, &format!("worker projection {i}"))),
            durability: Durability::Durable,
            routing: WriteRouting::Explicit(vec![relay.clone()]),
            identity: Identity::Active,
            correlation: None,
        }));
        let (id, generation, unsigned) = find_sign_request(&accepted);
        let signed = unsigned.sign_with_keys(&author).unwrap();
        let signed_effects = core.handle(EngineMsg::SignerCompleted(id, generation, Ok(signed)));
        assert!(
            signed_effects.iter().any(|effect| matches!(
                effect,
                Effect::EnsureWriteRelay(session)
                    if session == &signer_session(relay, author.public_key())
            )),
            "bootstrap must establish real worker demand before the read-count assertion"
        );
    }

    calls.set(0);
    let required = signer_session(&relays[0], author.public_key());
    // REPEATED unchanged passes, not one: the residual #985 names is that the
    // count grew by `N` on EVERY dispatch pass, so a single pass could hide a
    // per-pass cost behind a one-off.
    for pass in 0..5 {
        let effects = core.handle(EngineMsg::RelayOpenFailed(
            required.clone(),
            "injected worker-open failure".to_string(),
        ));

        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::EmitDiagnostics(_))),
            "the reducer must still recognize the projected worker as owned on pass {pass}"
        );
        assert_eq!(
            calls.get(),
            0,
            "unchanged worker-demand checks must use reducer memory; durable lane \
             reads belong to bootstrap/recovery and actual lane transitions. After \
             {} passes over {N} pending intents the old body would have read {}",
            pass + 1,
            (pass + 1) * N
        );
    }
}

/// The #968 parking property that #985's sequencing comment asks for, stated
/// now so it is testable before parking itself lands: `N` intents that have
/// never routed own no lane, so they contribute zero worker demand and cost
/// zero per-dispatch store reads.
///
/// An accepted-but-unsigned durable write is exactly the shape `AwaitingRoute`
/// will have -- a live obligation in `pending`, owning a durable intent row,
/// having minted no lane -- so the property is asserted against that shape
/// rather than against a lifecycle that does not exist yet. When #968 lands, a
/// parked write that somehow acquired worker demand fails here.
#[test]
fn route_parked_intents_add_no_worker_demand_and_no_store_reads() {
    const PARKED: usize = 6;
    let author = Keys::generate();
    let routed_relay = RelayUrl::parse("wss://parked-routed.example.com").unwrap();
    let parked_relay = RelayUrl::parse("wss://parked-unrouted.example.com").unwrap();

    let calls = Rc::new(Cell::new(0u64));
    let mut core = EngineCore::new(
        WakeLaneProbeStore::new(calls.clone()),
        Box::new(FixtureDirectory::new()),
        10,
    );
    activate(&mut core, &author);

    // One ordinary routed write, so the assertions below distinguish "parked
    // writes contribute nothing" from "this core computes nothing at all".
    let accepted = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(draft(400, "parked control")),
        durability: Durability::Durable,
        routing: WriteRouting::Explicit(vec![routed_relay.clone()]),
        identity: Identity::Active,
        correlation: None,
    }));
    let (id, generation, unsigned_event) = find_sign_request(&accepted);
    let signed = unsigned_event.sign_with_keys(&author).unwrap();
    let signed_effects = core.handle(EngineMsg::SignerCompleted(id, generation, Ok(signed)));
    assert!(
        signed_effects.iter().any(|effect| matches!(
            effect,
            Effect::EnsureWriteRelay(session)
                if session == &signer_session(&routed_relay, author.public_key())
        )),
        "the routed control write must establish real worker demand"
    );

    for i in 0..PARKED {
        let accepted = core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::Event(draft(500 + i as u64, &format!("parked {i}"))),
            durability: Durability::Durable,
            routing: WriteRouting::Explicit(vec![parked_relay.clone()]),
            identity: Identity::Active,
            correlation: None,
        }));
        assert!(
            accepted
                .iter()
                .any(|effect| matches!(effect, Effect::RequestSign(..))),
            "the parked-shape fixture must actually be accepted as a durable obligation"
        );
        assert!(
            !accepted
                .iter()
                .any(|effect| matches!(effect, Effect::EnsureWriteRelay(_))),
            "a write that has not routed yet must not claim a relay worker: {accepted:?}"
        );
    }

    calls.set(0);
    let parked_session = signer_session(&parked_relay, author.public_key());
    for pass in 0..5 {
        let effects = core.handle(EngineMsg::RelayOpenFailed(
            parked_session.clone(),
            "injected parked-relay open failure".to_string(),
        ));
        assert!(
            effects.is_empty(),
            "{PARKED} parked intents own no lane at {parked_relay}, so an open \
             failure there is not the engine's concern (pass {pass}): {effects:?}"
        );
    }

    let routed_session = signer_session(&routed_relay, author.public_key());
    for pass in 0..5 {
        let effects = core.handle(EngineMsg::RelayOpenFailed(
            routed_session.clone(),
            "injected routed-relay open failure".to_string(),
        ));
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::EmitDiagnostics(_))),
            "the one routed write still owns its worker on pass {pass}"
        );
    }

    assert_eq!(
        calls.get(),
        0,
        "{PARKED} parked intents plus ten unchanged dispatch passes must cost \
         zero recover_outbox_lanes calls"
    );
}

/// #985's hardest fail-closed shape: a lane CREATION whose post-state was
/// never observed. Retaining the previous projection is not enough, because
/// the lanes that may or may not have committed are NEW -- so every relay the
/// attempted bootstrap could have minted a lane for stays conservatively
/// owned until an explicit recovery proves otherwise. A false-positive worker
/// can be retired later; a false negative strands a durable obligation
/// forever.
///
/// `bootstrap_outbox_lanes` is both the create-if-missing mutation and the
/// one complete read that establishes the projection, so even a provably
/// `Absent` outcome (the injected fault here) does not prove that OLDER lanes
/// were absent.
#[test]
fn an_unknown_lane_creation_failure_retains_every_candidate_worker() {
    let author = Keys::generate();
    let relays: Vec<RelayUrl> = (0..3)
        .map(|i| RelayUrl::parse(&format!("wss://unproven-creation-{i}.example.com")).unwrap())
        .collect();

    let calls = Rc::new(Cell::new(0u64));
    let mut core = EngineCore::new(
        WakeLaneProbeStore::with_failing_bootstrap(calls.clone()),
        Box::new(FixtureDirectory::new()),
        10,
    );
    activate(&mut core, &author);

    let accepted = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(draft(600, "unproven lane creation")),
        durability: Durability::Durable,
        routing: WriteRouting::Explicit(relays.to_vec()),
        identity: Identity::Active,
        correlation: None,
    }));
    let (id, generation, unsigned_event) = find_sign_request(&accepted);
    let signed = unsigned_event.sign_with_keys(&author).unwrap();
    let signed_effects = core.handle(EngineMsg::SignerCompleted(id, generation, Ok(signed)));

    // Non-vacuity: the injected bootstrap failure really is the path taken,
    // so the ownership below cannot be coming from an ordinary lane. The one
    // delivery owner publishes every receipt fact as an effect, so the whole
    // accept-and-sign sequence is the exact status stream a receipt observer
    // would have seen.
    let statuses: Vec<WriteStatus> = receipt_statuses(&accepted)
        .into_iter()
        .chain(receipt_statuses(&signed_effects))
        .collect();
    for relay in &relays {
        assert!(
            statuses
                .iter()
                .any(|status| status == &WriteStatus::PersistenceBlocked(relay.clone())),
            "the fixture must actually take the failed-creation path for {relay}: {statuses:?}"
        );
    }

    for relay in &relays {
        let effects = core.handle(EngineMsg::RelayOpenFailed(
            signer_session(relay, author.public_key()),
            "injected open failure after an unprovable lane creation".to_string(),
        ));
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::EmitDiagnostics(_))),
            "{relay} was a candidate of the failed bootstrap, so it must stay \
             owned rather than lose its worker on an unproven creation"
        );
    }
}

/// Manual before/after harness for #985. Run in release mode on the base and
/// candidate revisions with the same constants:
///
/// `cargo test -p nmp --release --test core_headless
/// relay_worker_projection_redb_benchmark -- --ignored --nocapture`
///
/// No wall-clock threshold lives in CI; the behavioral regression above owns
/// correctness. This harness supplies empirical magnitude through the real
/// redb lane representation and the real `RelayOpenFailed` ownership path.
#[test]
#[ignore = "manual before/after performance qualification"]
fn relay_worker_projection_redb_benchmark() {
    const INTENTS: usize = 64;
    const PASSES: usize = 200;

    let author = Keys::generate();
    let relays: Vec<RelayUrl> = (0..INTENTS)
        .map(|i| RelayUrl::parse(&format!("wss://worker-benchmark-{i}.example.com")).unwrap())
        .collect();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("worker-projection-benchmark.redb");
    let mut core = EngineCore::new(
        RedbStore::open(&path).unwrap(),
        Box::new(FixtureDirectory::new()),
        INTENTS + 1,
    );
    activate(&mut core, &author);

    for (i, relay) in relays.iter().enumerate() {
        let accepted = core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::Event(draft(
                10_000 + i as u64,
                &format!("worker benchmark {i}"),
            )),
            durability: Durability::Durable,
            routing: WriteRouting::Explicit(vec![relay.clone()]),
            identity: Identity::Active,
            correlation: None,
        }));
        let (id, generation, unsigned) = find_sign_request(&accepted);
        let signed = unsigned.sign_with_keys(&author).unwrap();
        let effects = core.handle(EngineMsg::SignerCompleted(id, generation, Ok(signed)));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::EnsureWriteRelay(session)
                if session == &signer_session(relay, author.public_key())
        )));
    }

    let required = signer_session(&relays[0], author.public_key());
    let started = Instant::now();
    let mut diagnostic_batches = 0usize;
    for _ in 0..PASSES {
        let effects = core.handle(EngineMsg::RelayOpenFailed(
            required.clone(),
            "injected benchmark worker-open failure".to_string(),
        ));
        diagnostic_batches += effects
            .iter()
            .filter(|effect| matches!(effect, Effect::EmitDiagnostics(_)))
            .count();
        std::hint::black_box(effects);
    }
    let elapsed = started.elapsed();

    assert_eq!(diagnostic_batches, PASSES);
    println!(
        "relay_worker_projection_redb_benchmark intents={INTENTS} passes={PASSES} elapsed_us={}",
        elapsed.as_micros()
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
    let accepted1 = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(draft(200, "degraded 1")),
        durability: Durability::Durable,
        routing: WriteRouting::Explicit(vec![relay.clone()]),
        identity: Identity::Active,
        correlation: None,
    }));
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
    let accepted2 = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(draft(201, "degraded 2")),
        durability: Durability::Durable,
        routing: WriteRouting::Explicit(vec![relay.clone()]),
        identity: Identity::Active,
        correlation: None,
    }));
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
    // Two DISTINCT authors: `publish_explicit` freezes a fixed (seq, content)
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
        let (receipt_a, _event_a, scheduled_a) =
            publish_explicit(&mut core, &author_a, [relay_a.clone()]);
        mark_written(&mut core, &scheduled_a, &relay_a); // AckTimeout deadline = 10 + 30

        let _ = core.handle(EngineMsg::Tick(Timestamp::from(20)));
        let (receipt_b, _event_b, scheduled_b) =
            publish_explicit(&mut core, &author_b, [relay_b.clone()]);
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
    // `publish_explicit` freezes a fixed (seq, content) pair per call, so
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
    let (_receipt1, event1, first1) = publish_explicit(&mut core, &author1, [relay1.clone()]);
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
    let (receipt2, _event2, first2) = publish_explicit(&mut core, &author2, [relay2.clone()]);
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
