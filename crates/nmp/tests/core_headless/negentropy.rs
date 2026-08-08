use super::persistence_failures::FailIngestStore;
use super::*;

// ---- negentropy selection and fallback ---------------------------------

fn neg_err_frame(sub: &str) -> RelayFrame {
    RelayFrame::from(RelayMessage::NegErr {
        subscription_id: std::borrow::Cow::Owned(SubscriptionId::new(sub)),
        message: std::borrow::Cow::Owned("blocked: unsupported".to_string()),
    })
}

fn connect_and_prove_nip77<S: EventStore>(core: &mut EngineCore<S>, relay: &RelayUrl) {
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

/// Drive the exact edge the runtime drives when a relay worker ACCEPTS a
/// `NEG-OPEN` into its finite outbound envelope (#775).
///
/// The reducer holds this reconciliation's request evidence PENDING until the
/// outcome is known, because a connected generation is not an accepted frame.
/// Any headless test that later expects the reconciliation to settle therefore
/// has to place the frame the way production does -- the same discipline
/// `on_wire_request_handoff` already imposes on an ordinary REQ.
fn accept_neg_open<S: EventStore>(
    core: &mut EngineCore<S>,
    slot: u32,
    relay: &RelayUrl,
    neg_sub_id: &SubId,
) -> Vec<Effect> {
    core.on_nip77_handoff(
        Nip77Frame::Open,
        relay,
        neg_sub_id,
        Some(RelayHandle {
            slot,
            generation: 1,
        }),
        true,
        None,
    )
}

/// Drive a real server-side negentropy responder until the reducer opens the
/// id-targeted missing-event REQ. This keeps the failure test below on the
/// real reconciliation protocol rather than fabricating its internal state.
fn finish_neg_with_remote_event<S: EventStore>(
    core: &mut EngineCore<S>,
    slot: u32,
    relay: &RelayUrl,
    neg_sub_id: &SubId,
    initial_hex: &str,
    remote: &nostr::Event,
) -> SubId {
    let mut storage = ::negentropy::NegentropyStorageVector::new();
    storage
        .insert(
            remote.created_at.as_secs(),
            ::negentropy::Id::from_byte_array(*remote.id.as_bytes()),
        )
        .expect("insert remote negentropy item");
    storage.seal().expect("seal responder storage");
    let mut responder =
        ::negentropy::Negentropy::borrowed(&storage, 0).expect("construct responder");
    let mut client_hex = initial_hex.to_string();

    loop {
        let response = responder
            .reconcile(&hex::decode(&client_hex).expect("decode client negentropy message"))
            .expect("server-side reconciliation");
        let effects = core.handle(EngineMsg::RelayFrame(
            RelayHandle {
                slot,
                generation: 1,
            },
            public_session(relay),
            neg_msg_frame(&wire_sub_string(neg_sub_id), &hex::encode(response)),
        ));
        if let Some(backfill) = effects.iter().find_map(|effect| match effect {
            Effect::Wire(delta) => delta.ops.iter().find_map(|(session, ops)| {
                (session == &public_session(relay))
                    .then(|| {
                        ops.iter().find_map(|op| match op {
                            WireOp::Req(sub_id, filter)
                                if filter
                                    .ids
                                    .as_ref()
                                    .is_some_and(|ids| ids.contains(&remote.id.to_hex())) =>
                            {
                                Some(sub_id.clone())
                            }
                            _ => None,
                        })
                    })
                    .flatten()
            }),
            _ => None,
        }) {
            return backfill;
        }
        client_hex = effects
            .iter()
            .find_map(|effect| match effect {
                Effect::NegMsg(url, sub_id, next) if url == relay && sub_id == neg_sub_id => {
                    Some(next.clone())
                }
                _ => None,
            })
            .expect("reconciliation either continues or opens missing-id backfill");
    }
}

/// A row learned from relay A must not be advertised as a local holding to
/// relay B's NIP-77 reconciliation. Doing so makes the shared id compare as
/// equal, suppresses B's backfill, and permanently loses B provenance.
#[test]
fn negentropy_local_snapshot_is_scoped_to_the_reconciling_relay() {
    let original_author = Keys::generate();
    let widening_author = Keys::generate();
    let relay_a = RelayUrl::parse("wss://neg-source-a.example.com").unwrap();
    let relay_b = RelayUrl::parse("wss://neg-source-b.example.com").unwrap();
    let dir = FixtureRoutingFacts::new()
        .with_outbound_routes(
            original_author.public_key(),
            [relay_a.clone(), relay_b.clone()],
        )
        .with_outbound_routes(
            widening_author.public_key(),
            [relay_a.clone(), relay_b.clone()],
        );
    let mut core = new_core(dir);

    let initial = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &original_author.public_key().to_hex(),
    )));
    let relay_a_sub = req_for(&initial, &relay_a).0.clone();
    let _ = connect(&mut core, 0, &relay_a);
    let relay_b_connected = connect(&mut core, 1, &relay_b);
    let relay_b_probe = relay_b_connected
        .iter()
        .find_map(|effect| match effect {
            Effect::StartProbe(url, sub_id, ..) if url == &relay_b => Some(sub_id.clone()),
            _ => None,
        })
        .expect("relay B begins its NIP-77 capability probe");
    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 1,
            generation: 1,
        },
        public_session(&relay_b),
        neg_msg_frame(&wire_sub_string(&relay_b_probe), "6100"),
    ));

    let shared = nmp_resolver::testkit::kind1(
        &original_author,
        "same verified event exists at both relays",
        100,
    );
    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay_a),
        event_frame(&wire_sub_string(&relay_a_sub), shared.clone()),
    ));

    let widened = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &widening_author.public_key().to_hex(),
    )));
    let relay_b_live = req_for(&widened, &relay_b).0.clone();
    let opened = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 1,
            generation: 1,
        },
        public_session(&relay_b),
        eose_frame(&wire_sub_string(&relay_b_live)),
    ));
    let (neg_sub_id, initial_hex) = opened
        .iter()
        .find_map(|effect| match effect {
            Effect::NegOpen(_, sub_id, _, initial_hex) => {
                Some((sub_id.clone(), initial_hex.clone()))
            }
            _ => None,
        })
        .expect("relay B's live barrier opens its reconciliation");

    let backfill =
        finish_neg_with_remote_event(&mut core, 1, &relay_b, &neg_sub_id, &initial_hex, &shared);
    assert_ne!(
        backfill, relay_b_live,
        "relay B must request the shared id because NMP has not observed its copy"
    );
}

fn has_request_terminal(effects: &[Effect], terminal: RequestTerminal) -> bool {
    effects.iter().any(|effect| match effect {
        Effect::EmitObservationEvidence(_, evidence) => evidence.iter().any(|item| {
            matches!(
                item.fact,
                ObservationFact::RequestSettled {
                    terminal: candidate,
                    ..
                } if candidate == terminal
            )
        }),
        _ => false,
    })
}

/// Test 3 (ledger #8) first half: an unprobed relay (never even connected,
/// so its `Prober` state stays `Unknown`) must never see `Effect::NegOpen`
/// -- only a plain REQ.
#[test]
fn unprobed_relay_never_routes_to_negentropy() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureRoutingFacts::new().with_outbound_routes(a.public_key(), [relay0.clone()]);
    let mut core = new_core(dir);

    let effects = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &a.public_key().to_hex(),
    )));

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
    let dir = FixtureRoutingFacts::new().with_outbound_routes(a.public_key(), [relay0.clone()]);
    let mut core = new_core(dir);
    let subscribed = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &a.public_key().to_hex(),
    )));
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
    let _ = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &a.public_key().to_hex(),
    )));
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
    let dir = FixtureRoutingFacts::new().with_outbound_routes(a.public_key(), [relay0.clone()]);
    let mut core = new_core(dir);
    let _ = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &a.public_key().to_hex(),
    )));
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
    let dir = FixtureRoutingFacts::new().with_outbound_routes(a.public_key(), [relay0.clone()]);
    let mut core = new_core(dir);
    let _ = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &a.public_key().to_hex(),
    )));
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
    let dir = FixtureRoutingFacts::new().with_outbound_routes(a.public_key(), [relay0.clone()]);
    let mut core = new_core(dir);
    let _ = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &a.public_key().to_hex(),
    )));
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
    let mut core = new_core(FixtureRoutingFacts::new());
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
    let dir = FixtureRoutingFacts::new()
        .with_outbound_routes(a.public_key(), [relay0.clone()])
        .with_outbound_routes(b.public_key(), [relay0.clone()]);
    let mut core = new_core(dir);

    // Bootstrap: a's kind:1 atom -- the relay is `Unknown` at this point
    // (probing can only start once SOME demand causes a connection), so
    // this is unavoidably a plain REQ.
    let effects = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &a.public_key().to_hex(),
    )));
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
    let effects = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &b.public_key().to_hex(),
    )));
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
    let (neg_sub_id, initial_neg_hex) = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::NegOpen(_, sub_id, _, initial_hex) => {
                Some((sub_id.clone(), initial_hex.clone()))
            }
            _ => None,
        })
        .expect("the live EOSE barrier must open Negentropy");
    let _ = accept_neg_open(&mut core, 0, &relay0, &neg_sub_id);
    assert!(
        !has_request_terminal(&effects, RequestTerminal::Eose),
        "the limit:0 barrier opens NEG but does not settle the request"
    );
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::RecordCoverage(..))),
        "a limit:0 EOSE must never mint coverage even when the relay overdelivered"
    );
    assert_ne!(
        &neg_sub_id, &live_sub_id,
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

    // The relay has one stored event the local snapshot lacks. Completing
    // NEG must not settle until that exact id returns through the ordinary
    // backfill EVENT + EOSE path.
    use ::negentropy::{Id as NegId, Negentropy as RawNegentropy, NegentropyStorageVector};
    let missing = nmp_resolver::testkit::kind1(&b, "missing at NEG snapshot", 2);
    let mut relay_storage = NegentropyStorageVector::new();
    relay_storage
        .insert(
            missing.created_at.as_secs(),
            NegId::from_byte_array(*missing.id.as_bytes()),
        )
        .unwrap();
    relay_storage.seal().unwrap();
    let mut relay_side = RawNegentropy::owned(relay_storage, 0).unwrap();
    let mut client_message = initial_neg_hex;
    let completed = loop {
        let relay_reply = relay_side
            .reconcile(&hex::decode(&client_message).unwrap())
            .unwrap();
        let round = core.handle(EngineMsg::RelayFrame(
            RelayHandle {
                slot: 0,
                generation: 1,
            },
            public_session(&relay0),
            neg_msg_frame(&wire_sub_string(&neg_sub_id), &hex::encode(relay_reply)),
        ));
        if let Some(next) = round.iter().find_map(|effect| match effect {
            Effect::NegMsg(url, id, next) if url == &relay0 && id == &neg_sub_id => {
                Some(next.clone())
            }
            _ => None,
        }) {
            client_message = next;
            continue;
        }
        break round;
    };
    assert!(
        !has_request_terminal(&completed, RequestTerminal::Nip77),
        "NEG with missing ids is not settled before backfill"
    );
    let (backfill_id, backfill_filter) = req_for(&completed, &relay0);
    let backfill_id = backfill_id.clone();
    assert_eq!(
        backfill_filter.ids,
        Some(BTreeSet::from([missing.id.to_hex()])),
        "the backfill asks for exactly the missing relay id"
    );
    let ingested = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay0),
        event_frame(&wire_sub_string(&backfill_id), missing.clone()),
    ));
    assert!(
        ingested.iter().any(|effect| matches!(
            effect,
            Effect::EmitRows(_, rows, _) if rows.iter().any(|delta|
                matches!(delta, RowDelta::Added(row) if row.event.id == missing.id))
        )),
        "missing event is ingested before its backfill settles"
    );
    assert!(!has_request_terminal(&ingested, RequestTerminal::Nip77));
    let settled = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay0),
        eose_frame(&wire_sub_string(&backfill_id)),
    ));
    assert!(
        has_request_terminal(&settled, RequestTerminal::Nip77),
        "successful NIP-77 settles only after missing-id ingestion and EOSE"
    );

    // A second broad shape where both sides are empty settles directly at
    // NEG completion: no backfill REQ is invented, and the terminal is still
    // NIP-77 rather than the live-first barrier's EOSE.
    let empty_shape = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[2],
        &b.public_key().to_hex(),
    )));
    let (empty_live_id, empty_live_filter) = req_for(&empty_shape, &relay0);
    let empty_live_id = empty_live_id.clone();
    assert_eq!(empty_live_filter.limit, Some(0));
    let opened_empty = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay0),
        eose_frame(&wire_sub_string(&empty_live_id)),
    ));
    assert!(!has_request_terminal(&opened_empty, RequestTerminal::Eose));
    let (empty_neg_id, mut empty_client_message) = opened_empty
        .iter()
        .find_map(|effect| match effect {
            Effect::NegOpen(_, id, _, initial) => Some((id.clone(), initial.clone())),
            _ => None,
        })
        .expect("empty broad shape opens NEG after its live barrier");
    let _ = accept_neg_open(&mut core, 0, &relay0, &empty_neg_id);
    let mut empty_storage = NegentropyStorageVector::new();
    empty_storage.seal().unwrap();
    let mut empty_relay = RawNegentropy::owned(empty_storage, 0).unwrap();
    let empty_settled = loop {
        let reply = empty_relay
            .reconcile(&hex::decode(&empty_client_message).unwrap())
            .unwrap();
        let round = core.handle(EngineMsg::RelayFrame(
            RelayHandle {
                slot: 0,
                generation: 1,
            },
            public_session(&relay0),
            neg_msg_frame(&wire_sub_string(&empty_neg_id), &hex::encode(reply)),
        ));
        if let Some(next) = round.iter().find_map(|effect| match effect {
            Effect::NegMsg(url, id, next) if url == &relay0 && id == &empty_neg_id => {
                Some(next.clone())
            }
            _ => None,
        }) {
            empty_client_message = next;
            continue;
        }
        break round;
    };
    assert!(
        has_request_terminal(&empty_settled, RequestTerminal::Nip77),
        "NIP-77 with no missing ids settles at reconciliation completion"
    );
    assert!(
        empty_settled
            .iter()
            .all(|effect| !matches!(effect, Effect::Wire(_))),
        "no-missing completion opens no backfill request"
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
    let effects = core.handle_and_flush(EngineMsg::Subscribe(limited));
    req_for(&effects, &relay0); // must still be a plain REQ.
    assert!(
        !effects.iter().any(|e| matches!(e, Effect::NegOpen(..))),
        "a small/limited exact-result query must stay on REQ even for a Supported relay"
    );
}

/// #816's missing-id path falsifier. A real NIP-77 exchange proves one event
/// is absent locally and opens the ordinary id-targeted backfill REQ. If that
/// EVENT fails its durable commit, the backfill's EOSE must poison the
/// original NEG completion it was serving rather than minting coverage.
#[test]
fn failed_missing_id_event_commit_poisons_the_original_neg_completion() {
    let a = Keys::generate();
    let b = Keys::generate();
    let healthy = Keys::generate();
    let relay = RelayUrl::parse("wss://neg-failure.example.com").unwrap();
    let healthy_relay = RelayUrl::parse("wss://neg-healthy.example.com").unwrap();
    let dir = FixtureRoutingFacts::new()
        .with_outbound_routes(a.public_key(), [relay.clone()])
        .with_outbound_routes(b.public_key(), [relay.clone()])
        .with_outbound_routes(healthy.public_key(), [healthy_relay.clone()]);
    let mut core = EngineCore::new_with_fixture_routing_facts(FailIngestStore::armed(), dir, 10);

    let _ = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &a.public_key().to_hex(),
    )));
    let _ = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[2],
        &healthy.public_key().to_hex(),
    )));
    connect_and_prove_nip77(&mut core, &relay);
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
        .expect("the concurrent healthy request is already in flight");

    let widened = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &b.public_key().to_hex(),
    )));
    let candidate = req_for(&widened, &relay).0.clone();
    let opened = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay),
        eose_frame(&wire_sub_string(&candidate)),
    ));
    let (neg_sub_id, initial_hex) = opened
        .iter()
        .find_map(|effect| match effect {
            Effect::NegOpen(_, sub_id, _, initial_hex) => {
                Some((sub_id.clone(), initial_hex.clone()))
            }
            _ => None,
        })
        .expect("candidate EOSE opens the real NEG session");

    let missing = nmp_resolver::testkit::kind1(&b, "missing from local store", 100);
    let backfill =
        finish_neg_with_remote_event(&mut core, 0, &relay, &neg_sub_id, &initial_hex, &missing);
    let backfill_wire = wire_sub_string(&backfill);
    let _ = core.handle(EngineMsg::Tick(Timestamp::from(500u64)));
    let failed = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay),
        event_frame(&backfill_wire, missing),
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
        eose_frame(&backfill_wire),
    ));
    assert!(
        !completed
            .iter()
            .any(|effect| matches!(effect, Effect::RecordCoverage(..))),
        "the backfill EOSE must retire the poisoned NEG owner without coverage: {completed:?}"
    );
    assert!(
        !has_request_terminal(&completed, RequestTerminal::Nip77),
        "a failed backfill commit must not become NIP-77 absence evidence"
    );
    let atom_a = ctx_atom(cf(&[1], &[&a.public_key().to_hex()]));
    let atom_b = ctx_atom(cf(&[1], &[&b.public_key().to_hex()]));
    assert_eq!(
        core.get_coverage(&atom_a, &relay).expect("coverage peek"),
        None
    );
    assert_eq!(
        core.get_coverage(&atom_b, &relay).expect("coverage peek"),
        None
    );

    let healthy_event = nostr::EventBuilder::new(Kind::Custom(2), "healthy concurrent request")
        .custom_created_at(Timestamp::from(101u64))
        .sign_with_keys(&healthy)
        .expect("fixture signing");
    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 1,
            generation: 1,
        },
        public_session(&healthy_relay),
        event_frame(&wire_sub_string(&healthy_request), healthy_event),
    ));
    let healthy_completed = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 1,
            generation: 1,
        },
        public_session(&healthy_relay),
        eose_frame(&wire_sub_string(&healthy_request)),
    ));
    assert!(
        healthy_completed
            .iter()
            .any(|effect| matches!(effect, Effect::RecordCoverage(..))),
        "the missing-id failure must not poison a concurrent healthy request"
    );
    let healthy_atom = ctx_atom(cf(&[2], &[&healthy.public_key().to_hex()]));
    assert!(core
        .get_coverage(&healthy_atom, &healthy_relay)
        .expect("coverage peek")
        .is_some());
}

/// A relay that answers the capability probe with `NEG-ERR` is classified
/// `Unsupported` and cached -- its demand stays on plain REQ forever after,
/// same as an unprobed relay.
#[test]
fn relay_that_rejects_the_probe_is_classified_unsupported_and_stays_on_req() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureRoutingFacts::new().with_outbound_routes(a.public_key(), [relay0.clone()]);
    let mut core = new_core(dir);

    let effects = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &a.public_key().to_hex(),
    )));
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
    let effects = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &b.public_key().to_hex(),
    )));
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
    let dir = FixtureRoutingFacts::new()
        .with_outbound_routes(a.public_key(), [relay0.clone()])
        .with_outbound_routes(b.public_key(), [relay0.clone()]);
    let mut core = new_core(dir);

    let effects = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &a.public_key().to_hex(),
    )));
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

    let effects = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &b.public_key().to_hex(),
    )));
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
    let dir = FixtureRoutingFacts::new()
        .with_outbound_routes(a.public_key(), [relay.clone()])
        .with_outbound_routes(b.public_key(), [relay.clone()]);
    let mut core = new_core(dir);

    let initial = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &a.public_key().to_hex(),
    )));
    req_for(&initial, &relay);
    connect_and_prove_nip77(&mut core, &relay);

    let candidate = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &b.public_key().to_hex(),
    )));
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
    let dir = FixtureRoutingFacts::new()
        .with_outbound_routes(a.public_key(), [relay.clone()])
        .with_outbound_routes(b.public_key(), [relay.clone()]);
    let mut core = new_core(dir);

    let initial = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &a.public_key().to_hex(),
    )));
    let prior_live_id = req_for(&initial, &relay).0.clone();
    connect_and_prove_nip77(&mut core, &relay);
    let candidate = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &b.public_key().to_hex(),
    )));
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
    // processing. It closes the one-shot, but independent immutable live
    // requests remain open.
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
    assert!(!completed
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
    let dir = FixtureRoutingFacts::new()
        .with_outbound_routes(a.public_key(), [relay.clone()])
        .with_outbound_routes(b.public_key(), [relay.clone()]);
    let mut core = new_core(dir);

    let _ = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &a.public_key().to_hex(),
    )));
    connect_and_prove_nip77(&mut core, &relay);
    let candidate = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &b.public_key().to_hex(),
    )));
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
    let dir = FixtureRoutingFacts::new()
        .with_outbound_routes(a.public_key(), [relay.clone()])
        .with_outbound_routes(b.public_key(), [relay.clone()]);
    let mut core = new_core(dir);

    let initial = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &a.public_key().to_hex(),
    )));
    let a_handle = subscribed_handle(&initial);
    let a_live_id = req_for(&initial, &relay).0.clone();
    connect_and_prove_nip77(&mut core, &relay);
    let widened = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &b.public_key().to_hex(),
    )));
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

    // Removing b cancels only b's independent handoff. It never replaces
    // a's already-running request. Removing the final a owner closes that
    // incumbent.
    let narrowed = core.handle(EngineMsg::Unsubscribe(b_handle));
    assert!(narrowed.iter().any(|effect| matches!(effect,
        Effect::NegClose(url, id) if url == &relay && id == &neg_id
    )));
    assert!(wire_closes(&narrowed, &relay).contains(&live_id));
    assert!(narrowed.iter().all(|effect| !matches!(effect,
        Effect::Wire(delta) if delta.ops.iter().any(|(_, ops)| ops.iter().any(|op| matches!(op, WireOp::Req(..))))
    )));
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
    assert!(closed_ids.contains(&a_live_id));
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
    let dir = FixtureRoutingFacts::new().with_outbound_routes(a.public_key(), [relay.clone()]);
    let mut core = new_core(dir);

    let subscribed = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &a.public_key().to_hex(),
    )));
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

/// The same leak with another immutable request still active. Withdrawing
/// one demand must close that demand's fallback owners without disturbing
/// its sibling request.
#[test]
fn live_eose_timeout_withdrawal_closes_only_its_orphaned_candidate() {
    let a = Keys::generate();
    let b = Keys::generate();
    let relay = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureRoutingFacts::new()
        .with_outbound_routes(a.public_key(), [relay.clone()])
        .with_outbound_routes(b.public_key(), [relay.clone()]);
    let mut core = new_core(dir);

    let initial = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &a.public_key().to_hex(),
    )));
    let a_handle = subscribed_handle(&initial);
    connect_and_prove_nip77(&mut core, &relay);
    let widened = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &b.public_key().to_hex(),
    )));
    let b_handle = subscribed_handle(&widened);
    let live_sub_id = req_for(&widened, &relay).0.clone();

    // No candidate EOSE arrives before the liveness deadline -- same
    // fallback as above, with a's independent request still live.
    let timed_out = core.handle(EngineMsg::Tick(Timestamp::from(30u64)));
    let (backlog_id, _) = req_for(&timed_out, &relay);
    let backlog_id = backlog_id.clone();

    // Withdrawing b closes b's parked candidate and backlog. It must not
    // synthesize a replacement for unchanged a.
    let narrowed = core.handle(EngineMsg::Unsubscribe(b_handle));
    assert!(narrowed.iter().all(|effect| !matches!(effect,
        Effect::Wire(delta) if delta.ops.iter().any(|(_, ops)| ops.iter().any(|op| matches!(op, WireOp::Req(..))))
    )));
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
        "withdrawing demand mid-fallback must close the orphaned live \
         candidate REQ, or it leaks on the wire forever: {narrowed:?}"
    );
    assert!(
        narrowed_closed.contains(&backlog_id),
        "the backlog fallback REQ itself must close on withdrawal: {narrowed:?}"
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
        "a late EOSE on a withdrawn, orphaned candidate must never mint \
         phantom coverage: {late:?}"
    );

    let _ = core.handle(EngineMsg::Unsubscribe(a_handle));
}

// ---- #932: a reopened repair REQ never inherits a closed one's EOSE ------

/// THE #932 FALSIFIER, at the role-id path the allocated-wire-id work
/// (#899/PR #912) deliberately did not cover.
///
/// A NIP-77 role subscription's wire id used to be purely content-derived --
/// the router's plan id, a role byte, and the full filter hash -- while a
/// PLANNED subscription's id became an allocated token the router never
/// recycles. Content-derived means reproducible: close a repair REQ (its
/// inflight snapshots and its wire mapping are discarded), then re-derive the
/// same role for the same plan id and the same filter, and the identical
/// 64-hex string went back on the wire with a FRESH attribution FIFO. A
/// straggler EOSE for the PRE-CLOSE request then popped the reopened
/// request's snapshot and minted a durable coverage watermark for a REQ the
/// relay had not finished serving -- and coverage is exactly what
/// `plan_is_fresh_for` trusts, so the engine believed it held data that never
/// arrived.
///
/// The backlog fallback role is the sharpest instance because it is
/// deliberately UNLIMITED (so nothing poisons it) and carries the demand's
/// real absorbed keys (so its EOSE genuinely earns coverage).
///
/// Both legs matter. The stale EOSE must credit NOTHING, and the reopened
/// request's OWN EOSE must still credit normally -- an "exact attribution"
/// fix and a "dead attribution" regression are indistinguishable without the
/// second assertion.
#[test]
fn a_reopened_backlog_req_never_inherits_a_closed_incarnations_eose() {
    let a = Keys::generate();
    let b = Keys::generate();
    let relay = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureRoutingFacts::new()
        .with_outbound_routes(a.public_key(), [relay.clone()])
        .with_outbound_routes(b.public_key(), [relay.clone()]);
    let mut core = new_core(dir);

    let initial = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &a.public_key().to_hex(),
    )));
    let _ = subscribed_handle(&initial);
    connect_and_prove_nip77(&mut core, &relay);

    // A later immutable b request starts its own live-first handoff; letting
    // its liveness deadline expire parks an unlimited b-only backlog REQ.
    let widened = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &b.public_key().to_hex(),
    )));
    let b_handle = subscribed_handle(&widened);
    let timed_out = core.handle(EngineMsg::Tick(Timestamp::from(30u64)));
    let (first_backlog, backlog_filter) = req_for(&timed_out, &relay);
    let first_backlog = first_backlog.clone();
    assert_eq!(
        backlog_filter.limit, None,
        "the backlog fallback is unlimited, so its EOSE really does earn coverage"
    );

    // Withdrawing b closes that repair phase and discards its attribution.
    // Reopening b creates a new plan/request incarnation.
    let narrowed = core.handle(EngineMsg::Unsubscribe(b_handle));
    assert!(
        wire_closes(&narrowed, &relay).contains(&first_backlog),
        "narrowing must close the superseded backlog REQ: {narrowed:?}"
    );
    let rewidened = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &b.public_key().to_hex(),
    )));
    let _ = subscribed_handle(&rewidened);
    let timed_out_again = core.handle(EngineMsg::Tick(Timestamp::from(90u64)));
    let (reopened_backlog, _) = req_for(&timed_out_again, &relay);
    let reopened_backlog = reopened_backlog.clone();

    assert_ne!(
        reopened_backlog, first_backlog,
        "a reopened repair REQ must never go back on the wire under a \
         subscription id a closed one already used"
    );
    assert_eq!(
        wire_sub_string(&reopened_backlog).len(),
        64,
        "reincarnation must fit INSIDE the digest -- 64 hex characters is \
         exactly NIP-01's subscription_id cap and may never be exceeded"
    );

    // The straggler: the EOSE the relay finally sends for the request that
    // was closed at the narrowing step.
    let stale = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay),
        eose_frame(&wire_sub_string(&first_backlog)),
    ));
    assert!(
        !stale
            .iter()
            .any(|effect| matches!(effect, Effect::RecordCoverage(..))),
        "a straggler EOSE for a closed request must credit no coverage at \
         all -- the reopened request has not been served yet: {stale:?}"
    );
    let atom_a = ctx_atom(cf(&[1], &[&a.public_key().to_hex()]));
    let atom_b = ctx_atom(cf(&[1], &[&b.public_key().to_hex()]));
    assert_eq!(
        core.get_coverage(&atom_a, &relay).expect("coverage peek"),
        None
    );
    assert_eq!(
        core.get_coverage(&atom_b, &relay).expect("coverage peek"),
        None
    );

    // The positive leg: the reopened request's OWN EOSE still earns exactly
    // the coverage it proved.
    let served = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay),
        eose_frame(&wire_sub_string(&reopened_backlog)),
    ));
    assert!(
        served
            .iter()
            .any(|effect| matches!(effect, Effect::RecordCoverage(..))),
        "the reopened request's own EOSE must still record coverage -- \
         exact attribution, not dead attribution: {served:?}"
    );
    assert_eq!(
        core.get_coverage(&atom_a, &relay).expect("coverage peek"),
        None,
        "b's independent backlog cannot credit a"
    );
    assert!(core
        .get_coverage(&atom_b, &relay)
        .expect("coverage peek")
        .is_some());
}

/// The same reincarnation defect at the live-candidate role (#932). A
/// `limit:0` candidate poisons coverage by construction, so its observable is
/// the reconciliation barrier itself: its EOSE is what promotes it to the
/// live owner and opens Negentropy. Under a recycled wire id a straggler for
/// a closed candidate tripped that barrier for a candidate the relay had
/// never acknowledged.
#[test]
fn a_reopened_live_candidate_never_inherits_a_closed_incarnations_eose() {
    let a = Keys::generate();
    let b = Keys::generate();
    let relay = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureRoutingFacts::new()
        .with_outbound_routes(a.public_key(), [relay.clone()])
        .with_outbound_routes(b.public_key(), [relay.clone()]);
    let mut core = new_core(dir);

    let initial = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &a.public_key().to_hex(),
    )));
    let a_handle = subscribed_handle(&initial);
    connect_and_prove_nip77(&mut core, &relay);

    let widened = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &b.public_key().to_hex(),
    )));
    let b_handle = subscribed_handle(&widened);
    let candidate_b = req_for(&widened, &relay).0.clone();
    let opened = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay),
        eose_frame(&wire_sub_string(&candidate_b)),
    ));
    assert!(
        opened
            .iter()
            .any(|effect| matches!(effect, Effect::NegOpen(..))),
        "the candidate's own EOSE is the barrier that opens reconciliation"
    );

    // Withdrawing b closes its candidate/reconciliation without rewriting
    // a. Reopening b derives the same logical role under a fresh plan id.
    let narrowed = core.handle(EngineMsg::Unsubscribe(b_handle));
    assert!(wire_closes(&narrowed, &relay).contains(&candidate_b));
    let rewidened = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &b.public_key().to_hex(),
    )));
    let reopened_candidate = req_for(&rewidened, &relay).0.clone();

    assert_ne!(
        reopened_candidate, candidate_b,
        "a reopened live candidate must never reuse a closed candidate's \
         subscription id"
    );

    let stale = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay),
        eose_frame(&wire_sub_string(&candidate_b)),
    ));
    assert!(
        !stale
            .iter()
            .any(|effect| matches!(effect, Effect::NegOpen(..))),
        "a straggler EOSE for a closed candidate must never trip the \
         reconciliation barrier for the reopened one: {stale:?}"
    );

    // The reopened candidate's own EOSE still works exactly as before.
    let served = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay),
        eose_frame(&wire_sub_string(&reopened_candidate)),
    ));
    assert!(
        served
            .iter()
            .any(|effect| matches!(effect, Effect::NegOpen(..))),
        "the reopened candidate's own EOSE must still open reconciliation: \
         {served:?}"
    );
    let _ = core.handle(EngineMsg::Unsubscribe(a_handle));
}

// ---- transport refusal of a NIP-77 frame (#775) -------------------------
//
// `Pool::send` returns `false` when a relay worker refuses a frame at
// admission. #1331 made that refusal a fact about the worker's whole finite
// outbound envelope rather than one intermediate channel, so it is materially
// reachable: a relay that is redialing, or connected to a peer that has
// stopped reading, refuses every ordinary frame. The runtime used to discard
// it (`let _ = pool.send(..)`) at all three NIP-77 effects while the reducer
// state that produced the frame had already advanced. These drive the exact
// door the runtime drives, `EngineCore::on_nip77_handoff`.

/// A relay reachable enough to have its probe REFUSED locally is a relay NMP
/// has learned nothing about. Leaving it `Probing` wedges it for the whole
/// engine lifetime -- `Prober::begin_probe` only ever starts from `Unknown`
/// and there is no probe deadline -- so the relay silently never gets NIP-77
/// again, and diagnostics report `probing` forever.
#[test]
fn a_refused_probe_returns_the_relay_to_unknown_and_retires_its_wire_id() {
    let a = Keys::generate();
    let relay = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureRoutingFacts::new().with_outbound_routes(a.public_key(), [relay.clone()]);
    let mut core = new_core(dir);

    let effects = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &a.public_key().to_hex(),
    )));
    req_for(&effects, &relay);

    let connected = connect(&mut core, 0, &relay);
    let probe_sub = connected
        .iter()
        .find_map(|effect| match effect {
            Effect::StartProbe(url, sub_id, ..) if url == &relay => Some(sub_id.clone()),
            _ => None,
        })
        .expect("connecting a demanded, never-probed relay starts a capability probe");
    assert_eq!(
        core.diagnostics_snapshot().relays[0].nip77_behavior,
        "probing",
        "the probe is outstanding until transport reports its outcome"
    );

    // The exact edge the runtime reaches when `Pool::send` refuses the frame.
    let refused = core.on_nip77_handoff(
        Nip77Frame::Probe,
        &relay,
        &probe_sub,
        Some(RelayHandle {
            slot: 0,
            generation: 1,
        }),
        false,
        Some("transport send refused NIP-77 frame".to_string()),
    );

    assert_eq!(
        core.diagnostics_snapshot().relays[0].nip77_behavior,
        "unknown",
        "a frame that never left the process cannot leave the relay classified"
    );
    assert!(
        refused
            .iter()
            .any(|effect| matches!(effect, Effect::EmitDiagnostics(_))),
        "the changed capability verdict is observable state and must be published: {refused:?}"
    );

    // The retired wire id can no longer be satisfied: a frame arriving under
    // it (a straggler, or a relay answering a NEG-OPEN nobody sent) must not
    // mint behavioral proof.
    let straggler = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay),
        neg_msg_frame(&wire_sub_string(&probe_sub), "6100"),
    ));
    assert_eq!(
        core.diagnostics_snapshot().relays[0].nip77_behavior,
        "unknown",
        "a retired probe's wire id cannot mint behavioral proof: {straggler:?}"
    );
}

/// A refused `NEG-OPEN` must not be reported to the app as a placed request,
/// and must fall back immediately rather than waiting out the 30-second
/// silent-relay deadline for a frame that never reached a socket.
#[test]
fn a_refused_neg_open_never_claims_the_request_and_falls_back_immediately() {
    let a = Keys::generate();
    let b = Keys::generate();
    let relay = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureRoutingFacts::new()
        .with_outbound_routes(a.public_key(), [relay.clone()])
        .with_outbound_routes(b.public_key(), [relay.clone()]);
    let mut core = new_core(dir);

    let initial = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &a.public_key().to_hex(),
    )));
    req_for(&initial, &relay);
    connect_and_prove_nip77(&mut core, &relay);

    let candidate = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &b.public_key().to_hex(),
    )));
    let (live_sub_id, live_filter) = req_for(&candidate, &relay);
    let live_sub_id = live_sub_id.clone();
    assert_eq!(live_filter.limit, Some(0));

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
        .expect("the candidate EOSE opens reconciliation");
    assert!(
        !opened.iter().any(|effect| matches!(effect,
        Effect::EmitObservationEvidence(_, evidence)
            if evidence.iter().any(|item| matches!(
                &item.fact,
                ObservationFact::RelayRequest { .. }
            )))),
        "the reducer must not claim it placed the NEG-OPEN before transport \
         reports the outcome: {opened:?}"
    );

    let refused = core.on_nip77_handoff(
        Nip77Frame::Open,
        &relay,
        &neg_sub_id,
        Some(RelayHandle {
            slot: 0,
            generation: 1,
        }),
        false,
        Some("transport send refused NIP-77 frame".to_string()),
    );

    assert!(
        refused.iter().any(|effect| matches!(effect,
        Effect::EmitObservationEvidence(_, evidence)
            if evidence.iter().any(|item| matches!(
                &item.fact,
                ObservationFact::RelayRefused { relay: url, .. } if url == &relay
            )))),
        "a refused NIP-77 question is the one thing an app can see, and must be \
         emitted: {refused:?}"
    );
    let (fallback_id, fallback_filter) = req_for(&refused, &relay);
    assert_ne!(fallback_id, &neg_sub_id);
    assert_ne!(fallback_id, &live_sub_id);
    assert_eq!(fallback_filter.limit, None);
    assert_eq!(fallback_filter.since, None);
    assert_eq!(fallback_filter.until, None);
    assert!(
        !refused
            .iter()
            .any(|effect| matches!(effect, Effect::NegClose(..))),
        "the relay never saw a NEG-OPEN, so there is no reconciliation to \
         close: {refused:?}"
    );
    assert!(
        !refused
            .iter()
            .any(|effect| matches!(effect, Effect::Wire(delta)
            if delta.ops.iter().any(|(_, ops)| ops.iter().any(
                |op| matches!(op, WireOp::Close(id) if id == &live_sub_id))))),
        "the already-active live REQ stays open through the fallback: {refused:?}"
    );

    // Nothing is left for the liveness sweep to rediscover 30 seconds later.
    let swept = core.handle(EngineMsg::Tick(Timestamp::from(31u64)));
    assert!(
        !swept
            .iter()
            .any(|effect| matches!(effect, Effect::NegClose(_, id) if id == &neg_sub_id)),
        "the refusal already retired this reconciliation; the deadline has \
         nothing left to find: {swept:?}"
    );
}

/// A refused continuing `NEG-MSG` retires only that reconciliation, at once.
/// The reconciler has already consumed the relay's message and advanced, so
/// the exchange cannot be resumed -- but the relay did open this session, so
/// the best-effort `NEG-CLOSE` is still warranted.
#[test]
fn a_refused_neg_continue_falls_back_without_waiting_for_the_deadline() {
    let a = Keys::generate();
    let b = Keys::generate();
    let relay = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureRoutingFacts::new()
        .with_outbound_routes(a.public_key(), [relay.clone()])
        .with_outbound_routes(b.public_key(), [relay.clone()]);
    let mut core = new_core(dir);

    let initial = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &a.public_key().to_hex(),
    )));
    req_for(&initial, &relay);
    connect_and_prove_nip77(&mut core, &relay);

    let candidate = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &b.public_key().to_hex(),
    )));
    let (live_sub_id, _) = req_for(&candidate, &relay);
    let live_sub_id = live_sub_id.clone();
    let opened = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay),
        eose_frame(&wire_sub_string(&live_sub_id)),
    ));
    let (neg_sub_id, initial_hex) = opened
        .iter()
        .find_map(|effect| match effect {
            Effect::NegOpen(_, sub_id, _, hex) => Some((sub_id.clone(), hex.clone())),
            _ => None,
        })
        .expect("the candidate EOSE opens reconciliation");
    let accepted = core.on_nip77_handoff(
        Nip77Frame::Open,
        &relay,
        &neg_sub_id,
        Some(RelayHandle {
            slot: 0,
            generation: 1,
        }),
        true,
        None,
    );
    assert!(
        accepted.iter().any(|effect| matches!(effect,
        Effect::EmitObservationEvidence(_, evidence)
            if evidence.iter().any(|item| matches!(
                &item.fact,
                ObservationFact::RelayRequest { relay: url, .. } if url == &relay
            )))),
        "an ACCEPTED NEG-OPEN is exactly when the request becomes a placed \
         question: {accepted:?}"
    );

    // A real reconciliation round that does NOT finish: the responder holds
    // far more than one bounded frame can carry, so its first answer leaves
    // the exchange mid-flight and the reducer emits a continuing NEG-MSG.
    let mut storage = ::negentropy::NegentropyStorageVector::new();
    for index in 0u32..4000 {
        let mut id = [0u8; 32];
        id[..4].copy_from_slice(&index.to_be_bytes());
        storage
            .insert(u64::from(index) + 1, ::negentropy::Id::from_byte_array(id))
            .expect("insert responder item");
    }
    storage.seal().expect("seal responder storage");
    let mut responder =
        ::negentropy::Negentropy::borrowed(&storage, 4096).expect("construct bounded responder");
    let response = responder
        .reconcile(&hex::decode(&initial_hex).expect("decode initial message"))
        .expect("server-side reconciliation");
    let stepped = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay),
        neg_msg_frame(&wire_sub_string(&neg_sub_id), &hex::encode(response)),
    ));
    let continuing = stepped
        .iter()
        .find_map(|effect| match effect {
            Effect::NegMsg(_, sub_id, _) => Some(sub_id.clone()),
            _ => None,
        })
        .expect("a reconciliation round in progress emits a continuing NEG-MSG");
    assert_eq!(continuing, neg_sub_id);

    let refused = core.on_nip77_handoff(
        Nip77Frame::Continue,
        &relay,
        &neg_sub_id,
        Some(RelayHandle {
            slot: 0,
            generation: 1,
        }),
        false,
        Some("transport send refused NIP-77 frame".to_string()),
    );
    assert!(
        refused
            .iter()
            .any(|effect| matches!(effect, Effect::NegClose(url, id)
                if url == &relay && id == &neg_sub_id)),
        "the relay really did open this reconciliation, so it is closed \
         best-effort: {refused:?}"
    );
    let (fallback_id, fallback_filter) = req_for(&refused, &relay);
    assert_ne!(fallback_id, &neg_sub_id);
    assert_eq!(fallback_filter.limit, None);
    assert!(
        !refused
            .iter()
            .any(|effect| matches!(effect, Effect::Wire(delta)
            if delta.ops.iter().any(|(_, ops)| ops.iter().any(
                |op| matches!(op, WireOp::Close(id) if id == &live_sub_id))))),
        "the already-active live REQ stays open through the fallback: {refused:?}"
    );
    let swept = core.handle(EngineMsg::Tick(Timestamp::from(31u64)));
    assert!(
        !swept
            .iter()
            .any(|effect| matches!(effect, Effect::NegClose(_, id) if id == &neg_sub_id)),
        "the refusal already retired this reconciliation: {swept:?}"
    );
}
