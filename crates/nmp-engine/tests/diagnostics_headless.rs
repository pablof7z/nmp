//! Headless tests for the diagnostic surface (M5 plan §1.2/§1.4) -- "the
//! acceptance test made visible": per-relay wire-sub count, the exact wire
//! filters sent, events actually RECEIVED per (relay, kind), and per-filter
//! coverage. Zero I/O: every "relay" interaction here is a scripted
//! `EngineMsg::RelayConnected`/`RelayFrame` fed directly to
//! `EngineCore::handle`, exactly as `core_headless.rs` already does for the
//! read/write/coverage planes -- this file is the same discipline applied to
//! `EngineCore::diagnostics_snapshot`.

use std::collections::BTreeSet;

use nmp_engine::core::{Effect, EngineCore, EngineMsg, RowDelta, RowSink};
use nmp_grammar::{Binding, Filter};
use nmp_resolver::testkit::{kind1, kind3};
use nmp_resolver::LiveQuery;
use nmp_router::{FixtureDirectory, SubId, WireOp};
use nmp_store::MemoryStore;
use nmp_transport::{RelayFrame, RelayHandle};
use nostr::{
    EventBuilder, JsonUtil, Keys, Kind, RelayMessage, RelayUrl, SubscriptionId, Timestamp,
};

/// A `RowSink` that ignores everything -- these tests only care about
/// `Effect`s and `diagnostics_snapshot()`, never the row-delivery path
/// itself (already covered by `core_headless.rs`).
struct NullSink;
impl RowSink for NullSink {
    fn on_rows(&self, _rows: Vec<RowDelta>) {}
}

fn new_core(dir: FixtureDirectory) -> EngineCore<MemoryStore> {
    EngineCore::new(MemoryStore::new(), Box::new(dir), 10)
}

fn literal_query(kinds: &[u16], author_hex: &str) -> LiveQuery {
    LiveQuery(Filter {
        kinds: Some(kinds.iter().copied().collect()),
        authors: Some(Binding::Literal(BTreeSet::from([author_hex.to_string()]))),
        ..Filter::default()
    })
}

fn connect(core: &mut EngineCore<MemoryStore>, slot: u32, url: &RelayUrl) -> Vec<Effect> {
    core.handle(EngineMsg::RelayConnected(
        RelayHandle {
            slot,
            generation: 1,
        },
        url.clone(),
    ))
}

fn event_frame(sub: &str, event: nostr::Event) -> RelayFrame {
    RelayFrame::Text(RelayMessage::event(SubscriptionId::new(sub), event).as_json())
}

fn eose_frame(sub: &str) -> RelayFrame {
    RelayFrame::Text(RelayMessage::eose(SubscriptionId::new(sub)).as_json())
}

/// Find the single `WireOp::Req` sub-id opened for `relay` inside `effects`
/// -- test-fixture convenience (mirrors `core_headless.rs`'s `req_for`).
fn sub_id_for<'a>(effects: &'a [Effect], relay: &RelayUrl) -> &'a SubId {
    for effect in effects {
        if let Effect::Wire(delta) = effect {
            for (r, ops) in &delta.ops {
                if r == relay {
                    for op in ops {
                        if let WireOp::Req(sub_id, _) = op {
                            return sub_id;
                        }
                    }
                }
            }
        }
    }
    panic!("expected a WireOp::Req for {relay:?} in {effects:?}");
}

fn wire_sub_string(sub_id: &SubId) -> String {
    format!("{}", sub_id.1)
}

/// A kind:10002 (NIP-65 relay list) event -- genuinely missing from
/// `nmp_resolver::testkit`'s builder set (M5's self-bootstrapping outbox is
/// this crate's own concern, not M1's), so built directly here with no `r`
/// tags (this test only needs it to arrive and be COUNTED, never to actually
/// discover new write relays -- both authors' relays are already fixture-
/// known).
fn relay_list(author: &Keys, seq: u64) -> nostr::Event {
    EventBuilder::new(Kind::RelayList, "")
        .custom_created_at(Timestamp::from(seq))
        .sign_with_keys(author)
        .expect("test fixture event must sign cleanly")
}

/// The plan's headless falsifier (M5 plan §1.4): subscribe two authors, each
/// routed to their OWN relay; assert the diagnostics snapshot reports the
/// right per-relay sub count and the exact wire filter JSON; feed kind:3 +
/// kind:10002 + kind:1 events from specific relays and assert each bumps
/// exactly that relay's (relay, kind) counter -- never the other relay's.
#[test]
fn diagnostics_snapshot_reports_real_per_relay_subs_filters_and_per_kind_event_counts() {
    let me = Keys::generate();
    let friend = Keys::generate();
    let me_hex = me.public_key().to_hex();
    let friend_hex = friend.public_key().to_hex();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let relay1 = RelayUrl::parse("wss://relay1.example.com").unwrap();

    let dir = FixtureDirectory::new()
        .with_write(me_hex.clone(), [relay0.clone()])
        .with_write(friend_hex.clone(), [relay1.clone()]);
    let mut core = new_core(dir);

    connect(&mut core, 0, &relay0);
    connect(&mut core, 1, &relay1);

    let effects_a = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &me_hex),
        Box::new(NullSink),
    ));
    let sub0 = sub_id_for(&effects_a, &relay0).clone();

    let effects_b = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &friend_hex),
        Box::new(NullSink),
    ));
    let sub1 = sub_id_for(&effects_b, &relay1).clone();

    // ---- before any event: real sub counts + exact wire filters ---------
    let snap = core.diagnostics_snapshot();
    assert_eq!(snap.relays.len(), 2, "both relays must appear");

    let r0 = snap
        .relays
        .iter()
        .find(|r| r.relay == relay0)
        .expect("relay0 present");
    assert_eq!(r0.wire_sub_count, 1);
    assert_eq!(r0.filters.len(), 1);
    assert!(
        r0.filters[0].contains(&me_hex),
        "relay0's filter must be the EXACT wire JSON naming `me`, got: {}",
        r0.filters[0]
    );
    assert!(r0.filters[0].contains("\"kinds\":[1]"));
    assert!(
        r0.events_by_kind.is_empty(),
        "no events received yet -- must be empty, never fabricated"
    );

    let r1 = snap
        .relays
        .iter()
        .find(|r| r.relay == relay1)
        .expect("relay1 present");
    assert_eq!(r1.wire_sub_count, 1);
    assert!(r1.filters[0].contains(&friend_hex));

    // ---- feed kind:3 + kind:10002 + kind:1 from relay0 -------------------
    let wire0 = wire_sub_string(&sub0);
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        event_frame(&wire0, kind1(&me, "hello", 10)),
    ));
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::EmitDiagnostics(_))),
        "ingesting an event must push a fresh EmitDiagnostics reactively (D8: never polled)"
    );

    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        event_frame(&wire0, kind3(&me, &[friend.public_key()], 11)),
    ));
    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        event_frame(&wire0, relay_list(&me, 12)),
    ));
    // A second kind:1 from relay0 -- the counter must ACCUMULATE, not just
    // record presence.
    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        event_frame(&wire0, kind1(&me, "again", 13)),
    ));

    // ---- feed a kind:1 from relay1 only -----------------------------------
    let wire1 = wire_sub_string(&sub1);
    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 1,
            generation: 1,
        },
        event_frame(&wire1, kind1(&friend, "from friend", 20)),
    ));

    let snap = core.diagnostics_snapshot();
    let r0 = snap
        .relays
        .iter()
        .find(|r| r.relay == relay0)
        .expect("relay0 present");
    let r0_kinds: std::collections::BTreeMap<u16, u64> =
        r0.events_by_kind.iter().cloned().collect();
    assert_eq!(
        r0_kinds.get(&1).copied(),
        Some(2),
        "relay0 must show exactly 2 received kind:1 events"
    );
    assert_eq!(r0_kinds.get(&3).copied(), Some(1));
    assert_eq!(r0_kinds.get(&10_002).copied(), Some(1));

    let r1 = snap
        .relays
        .iter()
        .find(|r| r.relay == relay1)
        .expect("relay1 present");
    let r1_kinds: std::collections::BTreeMap<u16, u64> =
        r1.events_by_kind.iter().cloned().collect();
    assert_eq!(
        r1_kinds.get(&1).copied(),
        Some(1),
        "relay1's kind:1 event must be counted on relay1, never on relay0"
    );
    assert!(
        !r0_kinds.contains_key(&1) || r0_kinds[&1] != 3,
        "relay1's event must never bleed into relay0's counter"
    );
}

/// Per-filter coverage (M5 plan §1.1's "coverage per (filter, relay)"):
/// unproven (`None`) before any EOSE, a proven interval (`Some`) immediately
/// after -- and an `EmitDiagnostics` fires from the EOSE arm itself (which
/// never calls `recompile()`), proving the diagnostic surface observes
/// coverage change points that are not a recompile. Diagnostics is engine-
/// global and intentionally distinct from the query-facing
/// `AcquisitionEvidence` surface (`docs/design/scoped-evidence-49-12-plan.md`
/// §4) -- this asserts its own local fact (`Option<CoverageInterval>`), not
/// a query-level verdict.
#[test]
fn diagnostics_coverage_flips_none_to_proven_interval_on_eose_and_pushes_reactively() {
    let me = Keys::generate();
    let me_hex = me.public_key().to_hex();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();

    let dir = FixtureDirectory::new().with_write(me_hex.clone(), [relay0.clone()]);
    let mut core = new_core(dir);
    connect(&mut core, 0, &relay0);

    let effects = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &me_hex),
        Box::new(NullSink),
    ));
    let sub0 = sub_id_for(&effects, &relay0).clone();

    let snap = core.diagnostics_snapshot();
    let r0 = snap.relays.iter().find(|r| r.relay == relay0).unwrap();
    assert_eq!(r0.coverage.len(), 1);
    assert!(
        r0.coverage[0].coverage.is_none(),
        "no EOSE has arrived yet -- coverage must be unproven, never fabricated"
    );

    let wire0 = wire_sub_string(&sub0);
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        eose_frame(&wire0),
    ));
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::EmitDiagnostics(_))),
        "EOSE must push EmitDiagnostics even though this arm never calls recompile()"
    );

    let snap = core.diagnostics_snapshot();
    let r0 = snap.relays.iter().find(|r| r.relay == relay0).unwrap();
    assert!(
        r0.coverage[0].coverage.is_some(),
        "after EOSE the same filter's coverage must flip to a proven interval"
    );
}
