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

    let _ = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &a.public_key().to_hex(),
    )));

    let note = nmp_resolver_testkit::kind1(&a, "delete me", 100);
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
            if rows.iter().any(|r| matches!(r, RowDelta::Added(row) if row.id() == note_id)))),
        "the note must arrive as Added first"
    );

    let deletion = nmp_resolver_testkit::deletion(&a, &[note_id], 200);
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

    let _ = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &a.public_key().to_hex(),
    )));

    let expiring = nmp_resolver_testkit::expiring_kind1(&a, "ephemeral", 100, 150);
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
            if rows.iter().any(|r| matches!(r, RowDelta::Added(row) if row.id() == expiring_id)))),
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
