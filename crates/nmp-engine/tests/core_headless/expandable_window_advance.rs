//! #1886: the FIRST `request_rows` on an expandable window must place its
//! older-range REQ on the wire.
//!
//! Every other `attach_wire_handle` caller (`open_observation`,
//! `open_history_observation`, `withdraw_wire_demand`) ends by arming wire
//! admission, because retaining a wire atom only marks it PENDING —
//! `flush_wire_admission` is the transition that compiles pending atoms into
//! `Effect::Wire`, and the runtime runs it only when an `ArmWireAdmission`
//! effect armed its 10ms deadline. A staged advance that attaches handles and
//! never arms leaves its REQ pending indefinitely.
//!
//! The existing headless suite cannot catch this: `handle_and_flush` flushes
//! admission unconditionally, which supplies the very transition the bug
//! omits. These tests therefore flush only when the turn's own effects armed
//! it, exactly as `nmp-runtime`'s select loop does.

use nmp_engine::core::{HistoryQuery, HistorySessionId};

use super::*;

fn note(keys: &Keys, seq: u64, created_at: u64) -> nostr::Event {
    signed_draft(&draft(created_at, &format!("note-{seq}")), keys)
}

/// A window over one author's notes, pinned to `relay` so the plan needs no
/// routing facts at all.
fn window(author: &Keys, relay: &RelayUrl, initial: usize, max: usize) -> HistoryQuery {
    let demand = Demand::new(
        Filter {
            kinds: Some(BTreeSet::from([1u16])),
            authors: Some(Binding::Literal(BTreeSet::from([author
                .public_key()
                .to_hex()]))),
            ..Filter::default()
        },
        ReadRouting::Explicit(vec![relay.clone()]),
    )
    .expect("a pinned literal demand is valid");
    HistoryQuery::new(LiveQuery::single(demand), initial, max)
}

/// Seed `count` notes at `created_at` 100, 101, … and open a window over
/// them, leaving admission flushed so the next turn's arming is measured on
/// its own.
fn open_window(
    author: &Keys,
    relay: &RelayUrl,
    count: u64,
    initial: usize,
    max: usize,
) -> (EngineCore, HistorySessionId) {
    let mut store = RedbStore::temporary().expect("temporary Redb store");
    store
        .insert_batch(
            (0..count)
                .map(|seq| {
                    (
                        note(author, seq, 100 + seq),
                        RelayObserved::new(relay.clone(), Timestamp::from(1_000u64)),
                    )
                })
                .collect(),
        )
        .expect("seeding the window's local rows");
    let mut core = EngineCore::new_with_fixture_routing_facts(store, FixtureRoutingFacts::new(), 10);
    let opened = core.handle_and_flush(EngineMsg::SubscribeHistory(window(
        author, relay, initial, max,
    )));
    let id = opened
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitHistory(id, _) => Some(*id),
            _ => None,
        })
        .expect("opening a window emits its seed frame");
    (core, id)
}

/// `nmp-runtime`'s `Cmd::RequestRows` arm, reproduced: stage, accept the
/// staged result, then drive the post-commit continuation to convergence.
/// Returns every effect the runtime would have had in hand for that command.
fn request_rows(core: &mut EngineCore, id: HistorySessionId, at_least: usize) -> Vec<Effect> {
    let mut effects = core.handle(EngineMsg::RequestRows(id, at_least));
    assert!(
        effects.iter().any(|effect| matches!(
            effect,
            Effect::HistoryLoadResult(session, Ok(())) if *session == id
        )),
        "the staged advance must succeed before it can be committed"
    );
    let mut committed = core.handle(EngineMsg::CommitHistoryLoad(id));
    loop {
        let restaged = committed.iter().any(|effect| {
            matches!(
                effect,
                Effect::HistoryLoadResult(session, Ok(())) if *session == id
            )
        });
        effects.extend(committed);
        if !restaged {
            break;
        }
        committed = core.handle(EngineMsg::CommitHistoryLoad(id));
    }
    effects
}

/// The runtime flushes wire admission only from its armed deadline. Modeling
/// that faithfully is the whole point: an unconditional flush would hide the
/// missing arm.
fn flush_if_armed(core: &mut EngineCore, effects: &[Effect]) -> Vec<Effect> {
    if effects
        .iter()
        .any(|effect| matches!(effect, Effect::ArmWireAdmission))
    {
        core.handle(EngineMsg::FlushWireAdmission(Timestamp::from(0u64)))
    } else {
        Vec::new()
    }
}

/// Every `WireOp::Req` filter placed on `relay` by `effects`.
fn req_filters(effects: &[Effect], relay: &RelayUrl) -> Vec<ConcreteFilter> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Wire(delta) => Some(delta),
            _ => None,
        })
        .flat_map(|delta| delta.ops.iter())
        .filter(|(session, _)| &session.relay == relay)
        .flat_map(|(_, ops)| ops.iter())
        .filter_map(|op| match op {
            WireOp::Req(_, filter) => Some(filter.clone()),
            WireOp::Close(_) => None,
        })
        .collect()
}

#[test]
fn the_first_advance_places_its_older_range_req_on_the_wire() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://relay0.example.com").unwrap();
    // Six local rows at 100..=105; the window opens on the newest three, so
    // its boundary is 103 and the first advance asks strictly older than it.
    let (mut core, id) = open_window(&author, &relay, 6, 3, 6);

    let first = request_rows(&mut core, id, 6);
    let admitted = flush_if_armed(&mut core, &first);

    let filters = req_filters(&first, &relay)
        .into_iter()
        .chain(req_filters(&admitted, &relay))
        .collect::<Vec<_>>();
    assert!(
        filters.iter().any(|filter| filter.until == Some(102)),
        "the first advance owes the relay an older-range REQ until 102; \
         wire REQs placed were {filters:?}"
    );
}

#[test]
fn the_first_advance_arms_the_admission_that_compiles_its_pending_atoms() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let (mut core, id) = open_window(&author, &relay, 6, 3, 6);

    let first = request_rows(&mut core, id, 6);

    // The advance attached tie-second and older-range handles, so their atoms
    // are pending and only an armed admission can compile them.
    assert!(
        first
            .iter()
            .any(|effect| matches!(effect, Effect::ArmWireAdmission)),
        "an advance that attaches wire handles must arm admission; \
         the turn's effects were {first:?}"
    );
}
