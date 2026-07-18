use super::*;

// ---- negentropy selection and fallback ---------------------------------

fn neg_err_frame(sub: &str) -> RelayFrame {
    RelayFrame::from(RelayMessage::NegErr {
        subscription_id: std::borrow::Cow::Owned(SubscriptionId::new(sub)),
        message: std::borrow::Cow::Owned("blocked: unsupported".to_string()),
    })
}

fn connect_and_prove_nip77(core: &mut EngineCore<MemoryStore>, relay: &RelayUrl) {
    let effects = connect(core, 0, relay);
    let probe_sub = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::StartProbe(url, sub_id, ..) if url == relay => Some(sub_id),
            _ => None,
        })
        .expect("connected demanded relay must start its NIP-77 probe");
    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(relay),
        neg_msg_frame(&wire_sub_string(probe_sub), "6100"),
    ));
}

/// Test 3 (ledger #8) first half: an unprobed relay (never even connected,
/// so its `Prober` state stays `Unknown`) must never see `Effect::NegOpen`
/// -- only a plain REQ.
#[test]
fn unprobed_relay_never_routes_to_negentropy() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);

    let effects = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));

    assert!(
        !effects.iter().any(|e| matches!(e, Effect::NegOpen(..))),
        "an unprobed relay must never receive Effect::NegOpen -- only a plain REQ"
    );
    req_for(&effects, &relay0); // panics if there is no plain REQ.
}

#[test]
fn explicit_nip11_negative_suppresses_probe_without_minting_behavioral_proof() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);
    let subscribed = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let handle = subscribed
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitRows(handle, ..) => Some(*handle),
            _ => None,
        })
        .expect("subscribe emits the handle's initial row batch");

    let connected = core.handle(EngineMsg::RelayConnected(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay0),
    ));
    assert!(connected
        .iter()
        .any(|effect| matches!(effect, Effect::FetchRelayInformation(url) if url == &relay0)));

    let resolved = core.handle(EngineMsg::RelayInformationResolved(
        relay0.clone(),
        Some(nip11_evidence(Some(vec![11, 50]))),
    ));
    assert!(
        !resolved
            .iter()
            .any(|effect| matches!(effect, Effect::StartProbe(..) | Effect::NegOpen(..))),
        "advertised unsupported avoids a probe but cannot create a ProbedRelay"
    );
    let diagnostics = core.diagnostics_snapshot();
    let relay = diagnostics
        .relays
        .iter()
        .find(|relay| relay.relay == relay0)
        .expect("planned relay must be diagnosable");
    assert_eq!(relay.nip11_supported_nips, Some(vec![11, 50]));
    assert_eq!(
        relay.nip11_document_revision.as_deref(),
        Some("test-revision")
    );
    assert_eq!(relay.nip11_freshness, Some("fresh"));
    assert_eq!(relay.nip77_advertisement, "advertised_unsupported");
    assert_eq!(relay.nip77_behavior, "unknown");

    let _ = core.handle(EngineMsg::Unsubscribe(handle));
    let _ = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let replanned = core
        .diagnostics_snapshot()
        .relays
        .into_iter()
        .find(|relay| relay.relay == relay0)
        .expect("relay is planned again");
    assert_eq!(replanned.nip11_document_revision, None);
    assert_eq!(replanned.nip11_freshness, None);
    assert_eq!(replanned.nip77_advertisement, "unknown");
}

#[test]
fn positive_nip11_advertisement_starts_probe_but_is_not_behavioral_proof() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);
    let _ = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let _ = core.handle(EngineMsg::RelayConnected(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay0),
    ));

    let resolved = core.handle(EngineMsg::RelayInformationResolved(
        relay0.clone(),
        Some(nip11_evidence(Some(vec![11, 77]))),
    ));
    assert!(resolved
        .iter()
        .any(|effect| matches!(effect, Effect::StartProbe(url, ..) if url == &relay0)));
    assert!(!resolved
        .iter()
        .any(|effect| matches!(effect, Effect::NegOpen(..))));
    let diagnostics = core.diagnostics_snapshot();
    let relay = diagnostics
        .relays
        .iter()
        .find(|relay| relay.relay == relay0)
        .unwrap();
    assert_eq!(relay.nip77_advertisement, "advertised_supported");
    assert_eq!(relay.nip77_behavior, "probing");
}

#[test]
fn absent_supported_nips_is_proven_document_unknown_not_explicit_negative() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);
    let _ = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let _ = core.handle(EngineMsg::RelayConnected(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay0),
    ));

    let resolved = core.handle(EngineMsg::RelayInformationResolved(
        relay0.clone(),
        Some(nip11_evidence(None)),
    ));
    assert!(resolved
        .iter()
        .any(|effect| matches!(effect, Effect::StartProbe(url, ..) if url == &relay0)));
    let relay = core
        .diagnostics_snapshot()
        .relays
        .into_iter()
        .find(|relay| relay.relay == relay0)
        .unwrap();
    assert_eq!(relay.nip11_supported_nips, None);
    assert_eq!(
        relay.nip11_document_revision.as_deref(),
        Some("test-revision")
    );
    assert_eq!(relay.nip77_advertisement, "unknown");
    assert_eq!(relay.nip77_behavior, "probing");
}

#[test]
fn nip11_diagnostics_freshness_expires_from_engine_clock_without_another_acquisition() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);
    let _ = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let _ = core.handle(EngineMsg::Tick(Timestamp::from(100u64)));
    let _ = core.handle(EngineMsg::RelayInformationResolved(
        relay0.clone(),
        Some(nip11_evidence_until(Some(vec![11, 77]), 150)),
    ));

    let at_acquisition = core
        .diagnostics_snapshot()
        .relays
        .into_iter()
        .find(|relay| relay.relay == relay0)
        .unwrap();
    assert_eq!(at_acquisition.nip11_freshness, Some("fresh"));

    let _ = core.handle(EngineMsg::Tick(Timestamp::from(150u64)));
    let after_expiry = core
        .diagnostics_snapshot()
        .relays
        .into_iter()
        .find(|relay| relay.relay == relay0)
        .unwrap();
    assert_eq!(after_expiry.nip11_freshness, Some("stale"));
    assert_eq!(
        after_expiry.nip11_document_revision.as_deref(),
        Some("test-revision")
    );
}

/// #20 structural bypass falsifier: a transport connection notification is
/// not authority to create read work. Only a URL present in the current
/// compiled plan may be replayed or capability-probed.
#[test]
fn connected_relay_outside_the_compiled_plan_emits_no_read_wire_effect() {
    let mut core = new_core(FixtureDirectory::new());
    let unplanned = RelayUrl::parse("wss://unplanned.example.com").unwrap();

    let effects = core.handle(EngineMsg::RelayConnected(
        RelayHandle {
            slot: 7,
            generation: 1,
        },
        public_session(&unplanned),
    ));

    assert!(
        effects.is_empty(),
        "an unplanned connection must not mint replay/probe authority: {effects:?}"
    );
}

/// Test 3 (ledger #8) second half + test 10's routing half: drives the
/// Prober FSM to a real `Supported` verdict via a scripted NEG-MSG (exactly
/// what a real relay's probe response looks like from `EngineCore`'s point
/// of view), then proves a broad/unlimited demand change on that relay
/// routes through the gap-free live-first handoff while a small/limited
/// query on the SAME relay still stays on plain REQ.
#[test]
fn probed_relay_routes_broad_demand_to_negentropy_but_limited_demand_stays_on_req() {
    let a = Keys::generate();
    let b = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new()
        .with_write(a.public_key().to_hex(), [relay0.clone()])
        .with_write(b.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);

    // Bootstrap: a's kind:1 atom -- the relay is `Unknown` at this point
    // (probing can only start once SOME demand causes a connection), so
    // this is unavoidably a plain REQ.
    let effects = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    req_for(&effects, &relay0);

    let connect_effects = connect(&mut core, 0, &relay0);
    let (probe_sub, ..) = connect_effects
        .iter()
        .find_map(|e| match e {
            Effect::StartProbe(url, sub_id, filter, hex) if url == &relay0 => {
                Some((sub_id.clone(), filter.clone(), hex.clone()))
            }
            _ => None,
        })
        .expect("connecting a never-probed relay must start a capability probe");
    let probe_wire = wire_sub_string(&probe_sub);

    // The relay answers the probe with a NEG-MSG -- any valid response
    // classifies NIP-77 support; the payload's content is never inspected
    // by the prober.
    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay0),
        neg_msg_frame(&probe_wire, "6100"),
    ));

    // b's kind:1 atom widens the SAME (kind:1) skeleton -- same sub-id,
    // now the relay is Supported and the widened filter is broad
    // (unlimited), so it first opens a distinct live REQ with `limit:0`.
    let effects = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &b.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let (live_sub_id, live_filter) = req_for(&effects, &relay0);
    let live_sub_id = live_sub_id.clone();
    assert_eq!(live_filter.limit, Some(0));
    assert_eq!(
        core.diagnostics_snapshot().relays[0].nip77_handoff,
        "awaiting_live_eose"
    );
    assert!(
        !effects.iter().any(|e| matches!(e, Effect::NegOpen(..))),
        "NEG must wait until the candidate live REQ's exact EOSE"
    );

    // `limit:0` is a client request, not permission to trust relay
    // compliance. If a relay overdelivers a stored event before EOSE, the
    // canonical ingest path accepts/deduplicates it, while the limited EOSE
    // remains poisoned for coverage.
    let overdelivered = nmp_resolver::testkit::kind1(&b, "relay ignored limit zero", 1);
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay0),
        event_frame(&wire_sub_string(&live_sub_id), overdelivered.clone()),
    ));
    assert!(effects.iter().any(|effect| matches!(effect,
        Effect::EmitRows(_, rows, _) if rows.iter().any(|delta|
            matches!(delta, RowDelta::Added(row) if row.event.id == overdelivered.id))
    )));

    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay0),
        eose_frame(&wire_sub_string(&live_sub_id)),
    ));
    let neg_sub_id = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::NegOpen(_, sub_id, ..) => Some(sub_id),
            _ => None,
        })
        .expect("the live EOSE barrier must open Negentropy");
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::RecordCoverage(..))),
        "a limit:0 EOSE must never mint coverage even when the relay overdelivered"
    );
    assert_ne!(
        neg_sub_id, &live_sub_id,
        "REQ and NEG ids are separate namespaces"
    );
    assert_eq!(
        core.diagnostics_snapshot().relays[0].nip77_handoff,
        "reconciling"
    );
    assert!(
        !effects.iter().any(|e| matches!(e, Effect::Wire(d)
            if d.ops.iter().any(|(r, ops)| r.relay == relay0
                && ops.iter().any(|op| matches!(op, WireOp::Close(id) if id == &live_sub_id))))),
        "opening NEG must never close the active live REQ"
    );

    // The exact old failure window: reconciliation has snapshotted local
    // holdings, but has not completed. A newly-published event whose own
    // timestamp is old still arrives through the already-active live REQ;
    // a `since: now` tail would have lost it.
    let boundary = nmp_resolver::testkit::kind1(&b, "published during NEG", 1);
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay0),
        event_frame(&wire_sub_string(&live_sub_id), boundary.clone()),
    ));
    assert!(
        effects.iter().any(|effect| matches!(effect,
            Effect::EmitRows(_, rows, _) if rows.iter().any(|delta|
                matches!(delta, RowDelta::Added(row) if row.event.id == boundary.id))
        )),
        "the live-first handoff must deliver a backdated boundary event"
    );

    // A LIMITED (small-exact-result) query on the SAME relay stays on plain
    // REQ even though the relay is Supported -- ledger #8's REQ-fallback
    // selection rule (a different skeleton -- kind:7 -- so it is a brand
    // new, independent sub-id, unaffected by kind:1's negentropy routing).
    let limited = LiveQuery::from_filter(Filter {
        kinds: Some(BTreeSet::from([7u16])),
        authors: Some(Binding::Literal(BTreeSet::from([a.public_key().to_hex()]))),
        limit: Some(1),
        ..Filter::default()
    });
    let effects = core.handle(EngineMsg::Subscribe(
        limited,
        Box::new(CapturingSink::default()),
    ));
    req_for(&effects, &relay0); // must still be a plain REQ.
    assert!(
        !effects.iter().any(|e| matches!(e, Effect::NegOpen(..))),
        "a small/limited exact-result query must stay on REQ even for a Supported relay"
    );
}

/// A relay that answers the capability probe with `NEG-ERR` is classified
/// `Unsupported` and cached -- its demand stays on plain REQ forever after,
/// same as an unprobed relay.
#[test]
fn relay_that_rejects_the_probe_is_classified_unsupported_and_stays_on_req() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);

    let effects = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    req_for(&effects, &relay0);

    let connect_effects = connect(&mut core, 0, &relay0);
    let (probe_sub, ..) = connect_effects
        .iter()
        .find_map(|e| match e {
            Effect::StartProbe(url, sub_id, filter, hex) if url == &relay0 => {
                Some((sub_id.clone(), filter.clone(), hex.clone()))
            }
            _ => None,
        })
        .expect("connecting a never-probed relay must start a capability probe");
    let probe_wire = wire_sub_string(&probe_sub);

    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay0),
        neg_err_frame(&probe_wire),
    ));

    let b = Keys::generate();
    let effects = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &b.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    assert!(
        !effects.iter().any(|e| matches!(e, Effect::NegOpen(..))),
        "an Unsupported-classified relay must never route to negentropy"
    );
}

/// Structural grep-guard (ledger #8, "not a runtime `if`"): the ONLY place
/// in `core/mod.rs` that constructs a `ProbedRelay` value is inside
/// `negentropy/mod.rs` (`Prober::probed`/`Prober::on_neg_msg`) -- reading
/// `core/mod.rs`'s own source confirms it never spells the constructor
/// itself, so the only way it can ever hold one is by receiving it back
/// from `Prober`, exactly the compile-fence the plan asks for.
#[test]
fn core_never_constructs_a_probed_relay_directly() {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/core/mod.rs"))
        .expect("read core/mod.rs");
    let code_lines: Vec<&str> = src
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with("//"))
        .collect();
    assert!(
        !code_lines.iter().any(|l| l.contains("ProbedRelay(")),
        "core/mod.rs must never construct a ProbedRelay literal itself -- only `negentropy::Prober` may"
    );
}

/// Test 10's liveness half (bounded, headless): a reconciliation open past
/// [`NEG_LIVENESS_DEADLINE_SECS`]'s worth of synthetic clock advance is
/// abandoned and falls back to a plain REQ -- driven entirely via
/// `EngineCore::tick`'s own clock parameter, never a real sleep.
#[test]
fn stale_negentropy_session_falls_back_to_req_after_the_liveness_deadline() {
    let a = Keys::generate();
    let b = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new()
        .with_write(a.public_key().to_hex(), [relay0.clone()])
        .with_write(b.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);

    let effects = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    req_for(&effects, &relay0);

    let connect_effects = connect(&mut core, 0, &relay0);
    let (probe_sub, ..) = connect_effects
        .iter()
        .find_map(|e| match e {
            Effect::StartProbe(url, sub_id, filter, hex) if url == &relay0 => {
                Some((sub_id.clone(), filter.clone(), hex.clone()))
            }
            _ => None,
        })
        .expect("connecting a never-probed relay must start a capability probe");
    let probe_wire = wire_sub_string(&probe_sub);
    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay0),
        neg_msg_frame(&probe_wire, "6100"),
    ));

    let effects = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &b.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let (live_sub_id, live_filter) = req_for(&effects, &relay0);
    let live_sub_id = live_sub_id.clone();
    assert_eq!(live_filter.limit, Some(0));
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay0),
        eose_frame(&wire_sub_string(&live_sub_id)),
    ));
    let neg_sub_id = effects
        .iter()
        .find_map(|e| match e {
            Effect::NegOpen(_, sub_id, ..) => Some(sub_id.clone()),
            _ => None,
        })
        .expect("the candidate EOSE must open a negentropy session");

    // No reply ever arrives; advance the clock past the liveness deadline.
    let effects = core.handle(EngineMsg::Tick(Timestamp::from(31u64)));
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::NegClose(_, sub_id) if sub_id == &neg_sub_id)),
        "a stale session past the liveness deadline must be closed"
    );
    assert!(
        effects.iter().any(|e| matches!(e, Effect::Wire(d)
            if d.ops.iter().any(|(r, ops)| r.relay == relay0
                && ops.iter().any(|op| matches!(op, WireOp::Req(sid, filter)
                    if sid != &neg_sub_id && sid != &live_sub_id
                        && filter.limit.is_none()
                        && filter.since.is_none()
                        && filter.until.is_none()))))),
        "a stale session must fall back through a distinct unlimited backlog REQ"
    );
    assert!(
        !effects.iter().any(|e| matches!(e, Effect::Wire(d)
            if d.ops.iter().any(|(r, ops)| r.relay == relay0
                && ops.iter().any(|op| matches!(op, WireOp::Close(sid) if sid == &live_sub_id))))),
        "NEG timeout must leave the active live REQ open"
    );
}

#[test]
fn neg_err_falls_back_without_closing_the_active_live_req() {
    let a = Keys::generate();
    let b = Keys::generate();
    let relay = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new()
        .with_write(a.public_key().to_hex(), [relay.clone()])
        .with_write(b.public_key().to_hex(), [relay.clone()]);
    let mut core = new_core(dir);

    let initial = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    req_for(&initial, &relay);
    connect_and_prove_nip77(&mut core, &relay);

    let candidate = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &b.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let (live_sub_id, filter) = req_for(&candidate, &relay);
    let live_sub_id = live_sub_id.clone();
    assert_eq!(filter.limit, Some(0));
    let opened = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay),
        eose_frame(&wire_sub_string(&live_sub_id)),
    ));
    let neg_sub_id = opened
        .iter()
        .find_map(|effect| match effect {
            Effect::NegOpen(_, sub_id, ..) => Some(sub_id.clone()),
            _ => None,
        })
        .expect("candidate EOSE opens NEG");

    let failed = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay),
        neg_err_frame(&wire_sub_string(&neg_sub_id)),
    ));
    assert!(failed
        .iter()
        .any(|effect| matches!(effect, Effect::NegClose(url, id)
            if url == &relay && id == &neg_sub_id)));
    let (fallback_id, fallback_filter) = req_for(&failed, &relay);
    assert_ne!(fallback_id, &neg_sub_id);
    assert_ne!(fallback_id, &live_sub_id);
    assert_eq!(fallback_filter.limit, None);
    assert_eq!(fallback_filter.since, None);
    assert_eq!(fallback_filter.until, None);
    assert!(!failed
        .iter()
        .any(|effect| matches!(effect, Effect::Wire(delta)
            if delta.ops.iter().any(|(_, ops)| ops.iter().any(
                |op| matches!(op, WireOp::Close(id) if id == &live_sub_id)
            ))
        )));
    assert_eq!(
        core.diagnostics_snapshot().relays[0].nip77_handoff,
        "fallback_backlog"
    );
}

#[test]
fn live_eose_timeout_uses_a_distinct_backlog_and_keeps_overlap_until_proven() {
    let a = Keys::generate();
    let b = Keys::generate();
    let relay = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new()
        .with_write(a.public_key().to_hex(), [relay.clone()])
        .with_write(b.public_key().to_hex(), [relay.clone()]);
    let mut core = new_core(dir);

    let initial = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let prior_live_id = req_for(&initial, &relay).0.clone();
    connect_and_prove_nip77(&mut core, &relay);
    let candidate = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &b.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let live_sub_id = req_for(&candidate, &relay).0.clone();

    // No candidate EOSE arrives. At the exact deadline a separate full
    // backlog REQ starts; both old and new live subscriptions stay open.
    let timed_out = core.handle(EngineMsg::Tick(Timestamp::from(30u64)));
    let (backlog_id, backlog_filter) = req_for(&timed_out, &relay);
    let backlog_id = backlog_id.clone();
    assert_ne!(backlog_id, live_sub_id);
    assert_ne!(backlog_id, prior_live_id);
    assert_eq!(backlog_filter.limit, None);
    assert!(!timed_out
        .iter()
        .any(|effect| matches!(effect, Effect::Wire(delta)
            if delta.ops.iter().any(|(_, ops)| ops.iter().any(|op|
                matches!(op, WireOp::Close(id) if id == &live_sub_id || id == &prior_live_id)
            ))
        )));

    // EOSE for the later full request proves backlog delivery and ordered
    // processing. It closes the one-shot + predecessor, never the live
    // candidate that owns future delivery.
    let completed = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay),
        eose_frame(&wire_sub_string(&backlog_id)),
    ));
    assert!(completed
        .iter()
        .any(|effect| matches!(effect, Effect::Wire(delta)
            if delta.ops.iter().any(|(_, ops)| ops.iter().any(
                |op| matches!(op, WireOp::Close(id) if id == &backlog_id)
            ))
        )));
    assert!(completed
        .iter()
        .any(|effect| matches!(effect, Effect::Wire(delta)
            if delta.ops.iter().any(|(_, ops)| ops.iter().any(
                |op| matches!(op, WireOp::Close(id) if id == &prior_live_id)
            ))
        )));
    assert!(!completed
        .iter()
        .any(|effect| matches!(effect, Effect::Wire(delta)
            if delta.ops.iter().any(|(_, ops)| ops.iter().any(
                |op| matches!(op, WireOp::Close(id) if id == &live_sub_id)
            ))
        )));
    assert_eq!(core.diagnostics_snapshot().relays[0].nip77_handoff, "live");
}

#[test]
fn reconnect_repeats_live_first_and_only_the_fresh_generation_eose_opens_neg() {
    let a = Keys::generate();
    let b = Keys::generate();
    let relay = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new()
        .with_write(a.public_key().to_hex(), [relay.clone()])
        .with_write(b.public_key().to_hex(), [relay.clone()]);
    let mut core = new_core(dir);

    let _ = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    connect_and_prove_nip77(&mut core, &relay);
    let candidate = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &b.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let old_live_id = req_for(&candidate, &relay).0.clone();
    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay),
        eose_frame(&wire_sub_string(&old_live_id)),
    ));

    let _ = core.handle(EngineMsg::RelayDisconnected(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay),
        DisconnectReason::Error,
    ));
    let reconnected = core.handle(EngineMsg::RelayConnected(
        RelayHandle {
            slot: 0,
            generation: 2,
        },
        public_session(&relay),
    ));
    assert!(reconnected.iter().any(|effect| matches!(effect,
        Effect::Replay(session, reqs) if session == &public_session(&relay) && reqs.is_empty()
    )));
    let (fresh_live_id, fresh_filter) = req_for(&reconnected, &relay);
    let fresh_live_id = fresh_live_id.clone();
    assert_eq!(fresh_filter.limit, Some(0));
    assert!(!reconnected
        .iter()
        .any(|effect| matches!(effect, Effect::NegOpen(..))));

    let stale = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay),
        eose_frame(&wire_sub_string(&fresh_live_id)),
    ));
    assert!(stale.is_empty(), "old-generation EOSE must be inert");

    let fresh = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 2,
        },
        public_session(&relay),
        eose_frame(&wire_sub_string(&fresh_live_id)),
    ));
    assert!(fresh
        .iter()
        .any(|effect| matches!(effect, Effect::NegOpen(..))));
}

#[test]
fn withdrawing_all_demand_closes_live_candidate_and_every_repair_owner() {
    let a = Keys::generate();
    let b = Keys::generate();
    let relay = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new()
        .with_write(a.public_key().to_hex(), [relay.clone()])
        .with_write(b.public_key().to_hex(), [relay.clone()]);
    let mut core = new_core(dir);

    let initial = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let a_handle = subscribed_handle(&initial);
    connect_and_prove_nip77(&mut core, &relay);
    let widened = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &b.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let b_handle = subscribed_handle(&widened);
    let live_id = req_for(&widened, &relay).0.clone();
    let opened = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay),
        eose_frame(&wire_sub_string(&live_id)),
    ));
    let neg_id = opened
        .iter()
        .find_map(|effect| match effect {
            Effect::NegOpen(_, id, ..) => Some(id.clone()),
            _ => None,
        })
        .expect("candidate EOSE opens repair");

    // Removing b starts an overlap-safe replacement for a-only demand and
    // cancels the in-flight NEG. Removing the final a owner then closes both
    // that replacement candidate and the still-active predecessor.
    let narrowed = core.handle(EngineMsg::Unsubscribe(b_handle));
    assert!(narrowed.iter().any(|effect| matches!(effect,
        Effect::NegClose(url, id) if url == &relay && id == &neg_id
    )));
    let replacement_id = req_for(&narrowed, &relay).0.clone();
    let closed = core.handle(EngineMsg::Unsubscribe(a_handle));
    let closed_ids: BTreeSet<SubId> = closed
        .iter()
        .filter_map(|effect| match effect {
            Effect::Wire(delta) => Some(delta),
            _ => None,
        })
        .flat_map(|delta| delta.ops.iter().flat_map(|(_, ops)| ops))
        .filter_map(|op| match op {
            WireOp::Close(id) => Some(id.clone()),
            WireOp::Req(..) => None,
        })
        .collect();
    assert!(closed_ids.contains(&live_id));
    assert!(closed_ids.contains(&replacement_id));
    assert_eq!(core.diagnostics_snapshot().relays.len(), 0);
}

/// #570 follow-up: the `limit:0` live candidate REQ opened by
/// `begin_neg_handoff` is tracked only in `pending_neg_handoffs` until its
/// own EOSE arrives. If the liveness deadline fires FIRST (no candidate
/// EOSE), `handoff_fallback_to_req` moves it into
/// `TemporaryReq::BacklogActivatesLive`, deliberately keeping it open on
/// the wire while a distinct backlog REQ supplies a safe fallback -- now
/// tracked in NEITHER `pending_neg_handoffs` NOR `active_nip77_live`.
/// Withdrawing the only demand owner while still inside that fallback
/// window must still close and discard that orphaned candidate, or it
/// leaks forever and a later stray EOSE on its id mints phantom coverage.
#[test]
fn live_eose_timeout_fallback_then_full_withdrawal_closes_orphaned_candidate() {
    let a = Keys::generate();
    let relay = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay.clone()]);
    let mut core = new_core(dir);

    let subscribed = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let a_handle = subscribed_handle(&subscribed);
    connect_and_prove_nip77(&mut core, &relay);

    // A reconnect always replans live-first for whatever demand is
    // currently active (`reconnect_repeats_live_first_and_only_the_
    // fresh_generation_eose_opens_neg`), which is how a SINGLE demand
    // owner (no widen/narrow needed) ends up with its own `limit:0` live
    // candidate here.
    let _ = core.handle(EngineMsg::RelayDisconnected(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay),
        DisconnectReason::Error,
    ));
    let reconnected = core.handle(EngineMsg::RelayConnected(
        RelayHandle {
            slot: 0,
            generation: 2,
        },
        public_session(&relay),
    ));
    let (live_sub_id, live_filter) = req_for(&reconnected, &relay);
    let live_sub_id = live_sub_id.clone();
    assert_eq!(live_filter.limit, Some(0));

    // No candidate EOSE arrives before the liveness deadline: the
    // candidate is parked in `BacklogActivatesLive`, tracked in neither
    // `pending_neg_handoffs` nor `active_nip77_live`.
    let timed_out = core.handle(EngineMsg::Tick(Timestamp::from(30u64)));
    let (backlog_id, backlog_filter) = req_for(&timed_out, &relay);
    let backlog_id = backlog_id.clone();
    assert_ne!(backlog_id, live_sub_id);
    assert_eq!(backlog_filter.limit, None);

    // Withdraw the only demand owner while still inside that fallback
    // window, before the backlog REQ's own EOSE ever arrives.
    let closed = core.handle(EngineMsg::Unsubscribe(a_handle));
    let closed_ids: BTreeSet<SubId> = closed
        .iter()
        .filter_map(|effect| match effect {
            Effect::Wire(delta) => Some(delta),
            _ => None,
        })
        .flat_map(|delta| delta.ops.iter().flat_map(|(_, ops)| ops))
        .filter_map(|op| match op {
            WireOp::Close(id) => Some(id.clone()),
            WireOp::Req(..) => None,
        })
        .collect();
    assert!(
        closed_ids.contains(&live_sub_id),
        "withdrawing the only demand owner mid-fallback must close the \
         orphaned live candidate REQ, or it leaks on the wire forever: \
         {closed:?}"
    );
    assert!(
        closed_ids.contains(&backlog_id),
        "the backlog fallback REQ itself must still close on withdrawal: {closed:?}"
    );
    assert_eq!(core.diagnostics_snapshot().relays.len(), 0);

    // A late EOSE arriving on the orphaned candidate's wire id AFTER
    // withdrawal must never mint coverage for demand that no longer
    // exists. The connection is on generation 2 after the reconnect above.
    let late = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 2,
        },
        public_session(&relay),
        eose_frame(&wire_sub_string(&live_sub_id)),
    ));
    assert!(
        !late
            .iter()
            .any(|effect| matches!(effect, Effect::RecordCoverage(..))),
        "a late EOSE on a withdrawn, orphaned candidate must never mint \
         phantom coverage: {late:?}"
    );
}

/// Same leak, but demand is SUPERSEDED (narrowed) rather than fully
/// withdrawn while the plan sits in the live-EOSE-timeout fallback. The
/// narrowing itself runs through `begin_neg_handoff` ->
/// `cancel_nip77_repair_for_plan`, the exact call site of the fix -- distinct
/// from the full-withdrawal path's `close_nip77_plan`.
#[test]
fn live_eose_timeout_fallback_then_supersession_closes_orphaned_candidate() {
    let a = Keys::generate();
    let b = Keys::generate();
    let relay = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new()
        .with_write(a.public_key().to_hex(), [relay.clone()])
        .with_write(b.public_key().to_hex(), [relay.clone()]);
    let mut core = new_core(dir);

    let initial = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let a_handle = subscribed_handle(&initial);
    connect_and_prove_nip77(&mut core, &relay);
    let widened = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &b.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let b_handle = subscribed_handle(&widened);
    let live_sub_id = req_for(&widened, &relay).0.clone();

    // No candidate EOSE arrives before the liveness deadline -- same
    // fallback as above, but this plan still has two demand owners.
    let timed_out = core.handle(EngineMsg::Tick(Timestamp::from(30u64)));
    let (backlog_id, _) = req_for(&timed_out, &relay);
    let backlog_id = backlog_id.clone();

    // Narrowing back to a-only demand supersedes the still-parked
    // fallback via `begin_neg_handoff`'s own
    // `cancel_nip77_repair_for_plan` call -- it must close the orphaned
    // candidate too, not just the backlog REQ.
    let narrowed = core.handle(EngineMsg::Unsubscribe(b_handle));
    let replacement_id = req_for(&narrowed, &relay).0.clone();
    assert_ne!(replacement_id, live_sub_id);
    let narrowed_closed: BTreeSet<SubId> = narrowed
        .iter()
        .filter_map(|effect| match effect {
            Effect::Wire(delta) => Some(delta),
            _ => None,
        })
        .flat_map(|delta| delta.ops.iter().flat_map(|(_, ops)| ops))
        .filter_map(|op| match op {
            WireOp::Close(id) => Some(id.clone()),
            WireOp::Req(..) => None,
        })
        .collect();
    assert!(
        narrowed_closed.contains(&live_sub_id),
        "superseding demand mid-fallback must close the orphaned live \
         candidate REQ, or it leaks on the wire forever: {narrowed:?}"
    );
    assert!(
        narrowed_closed.contains(&backlog_id),
        "the backlog fallback REQ itself must still close on supersession: {narrowed:?}"
    );

    // A late EOSE on the orphaned candidate's wire id must never mint
    // coverage after it has been superseded away.
    let late = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay),
        eose_frame(&wire_sub_string(&live_sub_id)),
    ));
    assert!(
        !late
            .iter()
            .any(|effect| matches!(effect, Effect::RecordCoverage(..))),
        "a late EOSE on a superseded, orphaned candidate must never mint \
         phantom coverage: {late:?}"
    );

    let _ = core.handle(EngineMsg::Unsubscribe(a_handle));
}
