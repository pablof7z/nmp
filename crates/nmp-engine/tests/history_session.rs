use std::collections::{BTreeMap, BTreeSet};

use nmp_engine::core::{
    Effect, EngineCore, EngineMsg, HistoryBatch, HistoryQuery, HistorySessionId, HistorySink,
    RowDelta, WindowLoad,
};
use nmp_grammar::{Binding, Filter};
use nmp_resolver::LiveQuery;
use nmp_router::{FixtureDirectory, SubId, WireOp};
use nmp_store::{EventStore, MemoryStore, RelayObserved};
use nostr::{Event, Keys, Kind, RelayUrl, Timestamp, UnsignedEvent};

#[derive(Default)]
struct NullHistorySink;

impl HistorySink for NullHistorySink {
    fn on_history(&self, _batch: HistoryBatch) {}
}

fn signed(keys: &Keys, created_at: u64, content: &str) -> Event {
    UnsignedEvent::new(
        keys.public_key(),
        Timestamp::from(created_at),
        Kind::TextNote,
        Vec::new(),
        content,
    )
    .sign_with_keys(keys)
    .unwrap()
}

fn query(keys: &Keys, page_size: usize, max_rows: usize) -> HistoryQuery {
    HistoryQuery::new(
        LiveQuery::from_filter(Filter {
            kinds: Some(BTreeSet::from([1])),
            authors: Some(Binding::Literal(BTreeSet::from([keys
                .public_key()
                .to_hex()]))),
            ..Filter::default()
        }),
        page_size,
        max_rows,
    )
}

fn seeded(count: usize) -> (EngineCore<MemoryStore>, Keys, RelayUrl, Vec<Event>) {
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://history.example").unwrap();
    let mut events: Vec<_> = (0..count)
        .map(|index| signed(&keys, 100, &format!("same-second-{index}")))
        .collect();
    events.sort_by_key(|event| event.id);
    let mut store = MemoryStore::new();
    for event in &events {
        store
            .insert(
                event.clone(),
                RelayObserved::new(relay.clone(), Timestamp::from(200)),
            )
            .unwrap();
    }
    let directory = FixtureDirectory::new().with_write(keys.public_key().to_hex(), [relay.clone()]);
    (
        EngineCore::new(store, Box::new(directory), 10),
        keys,
        relay,
        events,
    )
}

fn returned(effects: &[Effect]) -> (HistorySessionId, HistoryBatch) {
    effects
        .iter()
        .rev()
        .find_map(|effect| match effect {
            Effect::EmitHistory(id, batch)
                if matches!(batch.load, WindowLoad::Idle | WindowLoad::Returned { .. }) =>
            {
                Some((*id, batch.clone()))
            }
            _ => None,
        })
        .expect("window operation emits a current batch")
}

fn open(effects: &[Effect]) -> HistorySessionId {
    effects
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitHistory(id, _) => Some(*id),
            _ => None,
        })
        .expect("subscribe emits the initial window frame")
}

fn apply(rows: &mut BTreeMap<nostr::EventId, Event>, batch: &HistoryBatch) {
    for delta in &batch.deltas {
        match delta {
            RowDelta::Added(row) => {
                assert!(rows.insert(row.event.id, row.event.clone()).is_none());
            }
            RowDelta::Removed(id) => {
                assert!(rows.remove(id).is_some());
            }
            RowDelta::SourcesGrew { .. } => {}
        }
    }
}

fn assert_canonical_snapshot(batch: &HistoryBatch, max_rows: usize) {
    assert!(batch.rows.len() <= max_rows);
    assert!(batch.rows.windows(2).all(|pair| {
        pair[0].event.created_at > pair[1].event.created_at
            || (pair[0].event.created_at == pair[1].event.created_at
                && pair[0].event.id < pair[1].event.id)
    }));
    assert_eq!(
        batch
            .rows
            .iter()
            .map(|row| row.event.id)
            .collect::<BTreeSet<_>>()
            .len(),
        batch.rows.len()
    );
}

/// The tie-second walk (#484) driven by the #485 declarative `request_rows`:
/// raising the target across a dense same-second boundary acquires the exact
/// tie-second proof once and the older range for the actual shortfall, with no
/// gap or duplicate.
#[test]
fn coordinated_session_walks_three_same_second_pages_without_gap_or_duplicate() {
    let (mut core, keys, relay, events) = seeded(13);
    let opened = core.handle(EngineMsg::SubscribeHistory(
        query(&keys, 5, 13),
        Box::new(NullHistorySink),
    ));
    let (id, first) = returned(&opened);
    assert_eq!(first.deltas.len(), 5);
    assert_eq!(first.rows.len(), 5);
    assert_canonical_snapshot(&first, 13);
    let mut rows = BTreeMap::new();
    apply(&mut rows, &first);

    // Raise the target to 10. Growth outcomes arrive at commit as a frame.
    let staged_second = core.handle(EngineMsg::RequestRows(id, 10));
    assert!(staged_second.iter().any(|effect| matches!(
        effect,
        Effect::HistoryLoadResult(session, Ok(())) if *session == id
    )));
    let second_effects = core.handle(EngineMsg::CommitHistoryLoad(id));
    let (_, second) = returned(&second_effects);
    assert_eq!(second.load, WindowLoad::Returned { added: 5 });
    assert_eq!(second.rows.len(), 10);
    assert_canonical_snapshot(&second, 13);
    apply(&mut rows, &second);
    assert_eq!(rows.len(), 10);

    // The advance acquires the exact tie-second proof (since==until==100, no
    // limit) and an older range bounded by the actual shortfall (5 rows).
    let reqs: Vec<_> = second_effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Wire(delta) => Some(delta),
            _ => None,
        })
        .flat_map(|delta| &delta.ops)
        .filter(|(url, _)| url == &relay)
        .flat_map(|(_, ops)| ops)
        .filter_map(|op| match op {
            WireOp::Req(_, filter) => Some(filter),
            WireOp::Close(_) => None,
        })
        .collect();
    assert!(reqs.iter().any(|filter| {
        filter.since == Some(100) && filter.until == Some(100) && filter.limit.is_none()
    }));
    assert!(reqs
        .iter()
        .any(|filter| filter.until == Some(99) && filter.limit == Some(5)));

    // request_rows is monotonic + idempotent + clamped: a value at or below the
    // current target is a pure no-op — no new frame, no staged load.
    for at_or_below in [4usize, 10usize] {
        let noop = core.handle(EngineMsg::RequestRows(id, at_or_below));
        assert!(noop.iter().any(|effect| matches!(
            effect,
            Effect::HistoryLoadResult(session, Ok(())) if *session == id
        )));
        assert!(!noop
            .iter()
            .any(|effect| matches!(effect, Effect::EmitHistory(session, _) if *session == id)));
        // No pending load and the window is unchanged.
        assert!(core.handle(EngineMsg::CommitHistoryLoad(id)).is_empty());
    }

    // Raise the target to the ceiling (13). The tie-second is already proven,
    // so only the older range for the 3-row shortfall is added.
    let staged_third = core.handle(EngineMsg::RequestRows(id, 13));
    assert!(staged_third.iter().any(|effect| matches!(
        effect,
        Effect::HistoryLoadResult(session, Ok(())) if *session == id
    )));
    let third_effects = core.handle(EngineMsg::CommitHistoryLoad(id));
    let (_, third) = returned(&third_effects);
    assert_eq!(third.load, WindowLoad::Returned { added: 3 });
    assert_eq!(third.rows.len(), 13);
    assert_canonical_snapshot(&third, 13);
    apply(&mut rows, &third);
    assert_eq!(rows.len(), 13);
    assert_eq!(
        rows.keys().copied().collect::<BTreeSet<_>>(),
        events.iter().map(|e| e.id).collect()
    );
}

/// At the declared ceiling, `request_rows` cannot grow the window — but the
/// caller still gets a delivered FACT (`AtBound`), never a thrown error. No
/// `Effect::HistoryLoadResult` is ever `Err` on this path.
#[test]
fn at_bound_is_a_delivered_frame_fact_not_an_error() {
    let (mut core, keys, _relay, _events) = seeded(4);
    // initial == max == 4: the window opens already at its ceiling.
    let opened = core.handle(EngineMsg::SubscribeHistory(
        query(&keys, 4, 4),
        Box::new(NullHistorySink),
    ));
    let id = open(&opened);

    let at_bound = core.handle(EngineMsg::RequestRows(id, 10));
    // Always Ok — being at the bound is not an error.
    assert!(at_bound.iter().any(|effect| matches!(
        effect,
        Effect::HistoryLoadResult(session, Ok(())) if *session == id
    )));
    assert!(!at_bound
        .iter()
        .any(|effect| matches!(effect, Effect::HistoryLoadResult(_, Err(_)))));

    // The AtBound beat is delivered through the normal staged commit path.
    let committed = core.handle(EngineMsg::CommitHistoryLoad(id));
    let beat = committed
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitHistory(session, batch) if *session == id => Some(batch),
            _ => None,
        })
        .expect("at-bound request emits one frame beat");
    assert_eq!(beat.load, WindowLoad::AtBound { max: 4 });
    assert!(!committed
        .iter()
        .any(|effect| matches!(effect, Effect::HistoryLoadResult(_, Err(_)))));
}

/// A `request_rows` that arrives while an advance is staged (not yet committed)
/// raises the target; committing the in-flight advance then auto-stages the
/// continuation so the session converges to the raised target.
#[test]
fn request_rows_during_in_flight_advance_converges_after_commit() {
    let (mut core, keys, _relay, _events) = seeded(13);
    let opened = core.handle(EngineMsg::SubscribeHistory(
        query(&keys, 5, 13),
        Box::new(NullHistorySink),
    ));
    let id = open(&opened);

    // Stage an advance toward 10, but do NOT commit yet.
    let staged = core.handle(EngineMsg::RequestRows(id, 10));
    assert!(staged.iter().any(|effect| matches!(
        effect,
        Effect::HistoryLoadResult(session, Ok(())) if *session == id
    )));

    // A second request while the first is in flight simply raises the target.
    let raised = core.handle(EngineMsg::RequestRows(id, 13));
    assert!(raised.iter().any(|effect| matches!(
        effect,
        Effect::HistoryLoadResult(session, Ok(())) if *session == id
    )));

    // Committing the in-flight advance delivers its 10-row frame AND auto-stages
    // the continuation toward the raised target of 13.
    let commit_one = core.handle(EngineMsg::CommitHistoryLoad(id));
    let (_, first) = returned(&commit_one);
    assert_eq!(first.rows.len(), 10);
    assert!(commit_one.iter().any(|effect| matches!(
        effect,
        Effect::HistoryLoadResult(session, Ok(())) if *session == id
    )));

    // Committing the auto-staged continuation converges the window to 13.
    let commit_two = core.handle(EngineMsg::CommitHistoryLoad(id));
    let (_, converged) = returned(&commit_two);
    assert_eq!(converged.rows.len(), 13);
    assert_eq!(converged.load, WindowLoad::Returned { added: 3 });

    // Fully converged: a further commit is a no-op.
    assert!(core.handle(EngineMsg::CommitHistoryLoad(id)).is_empty());
}

/// Unsubscribing a window releases every wire subscription the session placed
/// (initial + tie-second + older). Afterwards the session is gone and a stray
/// `request_rows` for it is a harmless no-op.
#[test]
fn unsubscribe_releases_every_session_subscription() {
    let (mut core, keys, relay, _events) = seeded(13);
    let opened = core.handle(EngineMsg::SubscribeHistory(
        query(&keys, 5, 13),
        Box::new(NullHistorySink),
    ));
    let id = open(&opened);
    core.handle(EngineMsg::RequestRows(id, 10));
    core.handle(EngineMsg::CommitHistoryLoad(id));

    let withdrawn = core.handle(EngineMsg::UnsubscribeHistory(id));
    let closes = withdrawn
        .iter()
        .filter_map(|effect| match effect {
            Effect::Wire(delta) => Some(delta),
            _ => None,
        })
        .flat_map(|delta| &delta.ops)
        .filter(|(url, _)| url == &relay)
        .flat_map(|(_, ops)| ops)
        .filter(|op| matches!(op, WireOp::Close(_)))
        .count();
    assert!(
        closes > 0,
        "withdrawing a window must close its placed wire subscriptions"
    );

    // The session is gone: a stray request is Ok and emits nothing.
    let stray = core.handle(EngineMsg::RequestRows(id, 13));
    assert!(stray.iter().any(|effect| matches!(
        effect,
        Effect::HistoryLoadResult(session, Ok(())) if *session == id
    )));
    assert!(!stray
        .iter()
        .any(|effect| matches!(effect, Effect::EmitHistory(session, _) if *session == id)));
}

/// Fold every wire op the effects place for `relay` into `open`, so `open`
/// always reflects the live subscription set: a `Req` opens a sub-id, a
/// `Close` retires it.
fn track_wire(open: &mut BTreeSet<SubId>, effects: &[Effect], relay: &RelayUrl) {
    for effect in effects {
        let Effect::Wire(delta) = effect else {
            continue;
        };
        for (url, ops) in &delta.ops {
            if url != relay {
                continue;
            }
            for op in ops {
                match op {
                    WireOp::Req(sub, _) => {
                        open.insert(sub.clone());
                    }
                    WireOp::Close(sub) => {
                        open.remove(sub);
                    }
                }
            }
        }
    }
}

/// #486 falsifier: a deep scroll of many advances must hold O(1) live relay
/// subscriptions, not one (or two) per advance. Every committed advance opens
/// an engine-owned tie-second and older-range acquisition REQ; without the
/// supersede-close each of `K` advances would leak its acquisitions until
/// teardown (`2 * (max_rows / page_size) + 1` concurrent REQs per relay for a
/// fully scrolled session — enough to trip a relay's per-connection sub cap).
/// The engine must retire each prior advance's historical acquisitions at the
/// next commit, keeping only the permanent live-top demand plus the current
/// advance's own handles, so the net open subscription count per relay stays
/// bounded no matter how deep the scroll runs.
#[test]
fn deep_scroll_holds_bounded_live_subscriptions_per_relay() {
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://history.example").unwrap();
    let mut store = MemoryStore::new();
    // Strictly descending, distinct-second rows so every advance genuinely
    // opens a fresh tie-second AND older-range acquisition.
    let total = 210usize;
    for index in 0..total {
        let created_at = 10_000 - index as u64;
        store
            .insert(
                signed(&keys, created_at, &format!("row-{index}")),
                RelayObserved::new(relay.clone(), Timestamp::from(20_000)),
            )
            .unwrap();
    }
    let directory = FixtureDirectory::new().with_write(keys.public_key().to_hex(), [relay.clone()]);
    let mut core = EngineCore::new(store, Box::new(directory), 10);

    let opened = core.handle(EngineMsg::SubscribeHistory(
        query(&keys, 5, 1000),
        Box::new(NullHistorySink),
    ));
    let id = open(&opened);

    let mut live_subs = BTreeSet::new();
    track_wire(&mut live_subs, &opened, &relay);

    let mut peak = live_subs.len();
    // 40 advances of one page (5 rows) each — a 200-row deep scroll. Without
    // the fix this would accumulate ~80 leaked historical REQs on `relay`.
    for step in 1..=40 {
        let target = 5 + step * 5;
        let staged = core.handle(EngineMsg::RequestRows(id, target));
        track_wire(&mut live_subs, &staged, &relay);
        // Drive the auto-staged continuation to convergence, exactly as the
        // runtime commit loop does, folding every advance's wire ops.
        loop {
            let committed = core.handle(EngineMsg::CommitHistoryLoad(id));
            if committed.is_empty() {
                break;
            }
            track_wire(&mut live_subs, &committed, &relay);
            peak = peak.max(live_subs.len());
            let restaged = committed.iter().any(|effect| {
                matches!(effect, Effect::HistoryLoadResult(session, Ok(())) if *session == id)
            });
            if !restaged {
                break;
            }
        }
        peak = peak.max(live_subs.len());
    }

    // Live-top demand + at most the current advance's tie-second and older
    // acquisitions: a small constant, never O(number of advances).
    assert!(
        peak <= 3,
        "deep scroll must hold O(1) live subscriptions per relay across many \
         advances, but peaked at {peak} concurrent REQs (the #486 leak)"
    );
    // And after the whole scroll the session still holds only that bounded set.
    assert!(
        live_subs.len() <= 3,
        "a fully scrolled session must not retain a subscription per advance, \
         holds {} REQs",
        live_subs.len()
    );
}
