use super::*;

// ---- retraction, expiry, deadlines, and inbox routing -------------------

/// `RowDelta::Removed` on kind:5 deletion (issue #34's `root_query_emits_
/// removed_on_delete` obligation, asserted explicitly here even though it
/// "may already pass via refresh's full-set diff" -- a root query has no
/// `Derived` node to seed at all, so the row simply leaving the store on
/// the next `refresh_handle` is enough; the resolver-level dirty-seed
/// wiring this issue adds is what makes the SAME delete also retract a
/// `Derived` member correctly, covered separately in
/// `nmp-resolver/tests/contract.rs`).
#[test]
fn root_query_emits_removed_on_delete() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureRoutingFacts::new().with_outbound_routes(a.public_key(), [relay0.clone()]);
    let mut core = new_core(dir);
    connect(&mut core, 0, &relay0);

    let _ = core.handle(EngineMsg::Subscribe(literal_query(
        &[1],
        &a.public_key().to_hex(),
    )));

    let note = nmp_resolver::testkit::kind1(&a, "delete me", 100);
    let note_id = note.id;
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay0),
        event_frame("s", note),
    ));
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::EmitRows(_, rows, _)
            if rows.iter().any(|r| matches!(r, RowDelta::Added(row) if row.event.id == note_id)))),
        "the note must arrive as Added first"
    );

    let deletion = nmp_resolver::testkit::deletion(&a, &[note_id], 200);
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay0),
        event_frame("s", deletion),
    ));

    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::EmitRows(_, rows, _)
            if rows.iter().any(|r| matches!(r, RowDelta::Removed(id) if *id == note_id)))),
        "a kind:5 delete of a row the handle is currently holding must emit \
         RowDelta::Removed for it: {effects:?}"
    );
}

/// NIP-40 expiry retraction (issue #34's `expiry_emits_removed_via_manual_
/// tick`, retraction-and-negative-deltas.md §3.2): a manual/synthetic-clock
/// `EngineMsg::Tick` drains `store.expire_due`, routes the removed row
/// through `resolver.retract`, and the ordinary refresh diff emits
/// `RowDelta::Removed` -- with zero further input (no new event arrives,
/// only the clock advancing). This proves the mechanism directly, against a
/// synthetic clock, independent of who calls `tick` -- the `recv_timeout`
/// runtime driver that now fires this on its own live (#39, design §3.3) is
/// exercised separately in `runtime_integration.rs`.
#[test]
fn expiry_emits_removed_via_manual_tick() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureRoutingFacts::new().with_outbound_routes(a.public_key(), [relay0.clone()]);
    let mut core = new_core(dir);
    connect(&mut core, 0, &relay0);

    let _ = core.handle(EngineMsg::Subscribe(literal_query(
        &[1],
        &a.public_key().to_hex(),
    )));

    let expiring = nmp_resolver::testkit::expiring_kind1(&a, "ephemeral", 100, 150);
    let expiring_id = expiring.id;
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay0),
        event_frame("s", expiring),
    ));
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::EmitRows(_, rows, _)
            if rows.iter().any(|r| matches!(r, RowDelta::Added(row) if row.event.id == expiring_id)))),
        "the expiring note must arrive as Added first"
    );

    // No further event arrives -- only the clock advances past its
    // expiration deadline (150).
    let effects = core.handle(EngineMsg::Tick(Timestamp::from(200u64)));

    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::EmitRows(_, rows, _)
            if rows.iter().any(|r| matches!(r, RowDelta::Removed(id) if *id == expiring_id)))),
        "tick() past the expiration deadline must emit RowDelta::Removed \
         with no new event: {effects:?}"
    );
}

/// #39 / retraction-and-negative-deltas.md §3.2: `EngineCore::next_deadline`
/// is the min over every deadline source this reducer currently tracks --
/// NIP-40 expiry (`store.next_expiration()`) and open negentropy sessions'
/// liveness deadlines (`started_at + NEG_LIVENESS_DEADLINE_SECS`, the same
/// 30s constant `stale_negentropy_session_falls_back_to_req_after_the_
/// liveness_deadline` exercises). Entirely against a synthetic clock -- no
/// real time elapses in this test -- so it is a pure function of `core`'s
/// tracked state, exactly what the `runtime::engine_loop` driver (tested
/// live in `runtime_integration.rs`) re-reads every iteration to arm its
/// `recv_timeout`.
#[test]
fn next_deadline_is_min_over_expiry_and_neg_liveness() {
    let a = Keys::generate();
    let b = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureRoutingFacts::new()
        .with_outbound_routes(a.public_key(), [relay0.clone()])
        .with_outbound_routes(b.public_key(), [relay0.clone()]);
    let mut core = new_core(dir);

    assert_eq!(
        core.next_deadline(),
        None,
        "a fresh core tracks no expiring events and no open neg session"
    );

    let _ = core.handle(EngineMsg::Subscribe(literal_query(
        &[1],
        &a.public_key().to_hex(),
    )));
    let connect_effects = connect(&mut core, 0, &relay0);

    // Ingest an event expiring at t=150 on the open sub -- the store's
    // expiration index is now the sole deadline source (no neg session
    // exists yet).
    let expiring = nmp_resolver::testkit::expiring_kind1(&a, "ephemeral", 100, 150);
    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay0),
        event_frame("s", expiring),
    ));
    assert_eq!(
        core.next_deadline(),
        Some(Timestamp::from(150u64)),
        "with only an expiring event, next_deadline is the store's expiry"
    );

    // Drive the SAME probe-then-widen dance as
    // `probed_relay_routes_broad_demand_to_negentropy_but_limited_demand_
    // stays_on_req` to open a real neg session on relay0.
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
    let effects = core.handle(EngineMsg::Subscribe(literal_query(
        &[1],
        &b.public_key().to_hex(),
    )));
    let (live_sub_id, live_filter) = req_for(&effects, &relay0);
    assert_eq!(live_filter.limit, Some(0));
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay0),
        eose_frame(&wire_sub_string(live_sub_id)),
    ));
    assert!(
        effects.iter().any(|e| matches!(e, Effect::NegOpen(..))),
        "setup: the candidate live EOSE must actually open a neg session"
    );

    // `NegSession::started_at` is `core`'s clock, which nothing above has
    // advanced past `EngineCore::new`'s default of 0 (only `Tick` ever
    // moves it) -- so the neg-liveness deadline lands at exactly
    // NEG_LIVENESS_DEADLINE_SECS (30), strictly nearer than the expiry at
    // 150, and must win the min.
    assert_eq!(
        core.next_deadline(),
        Some(Timestamp::from(30u64)),
        "an open neg session's liveness deadline (30) is nearer than the \
         expiry (150) and must win the min"
    );
}
