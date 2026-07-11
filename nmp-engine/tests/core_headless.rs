//! Headless `EngineCore` tests (M3 plan §5 tier A, re-expressed at the
//! `EngineCore` level per the M3-B build brief) + the coverage-attribution
//! ruling's falsifiers
//! (`docs/consults/2026-07-11-fable-coverage-attribution.md`). Zero I/O:
//! every "relay" interaction here is a scripted `EngineMsg::RelayConnected`/
//! `RelayFrame` fed directly to `EngineCore::handle`, exactly as the ruling's
//! own reasoning demands (send-time snapshots, the EOSE intersection rule,
//! `limit` poisoning, and per-query `CompleteUpTo`/`Unknown` aggregation).

use std::sync::{Arc, Mutex};

use nmp_engine::core::{Effect, EngineCore, EngineMsg, QueryCoverage, RowDelta, RowSink};
use nmp_grammar::{Binding, ConcreteFilter, Filter};
use nmp_resolver::LiveQuery;
use nmp_router::{FixtureDirectory, SubId, WireOp};
use nmp_store::MemoryStore;
use nmp_transport::{RelayFrame, RelayHandle};
use nostr::{JsonUtil, Keys, RelayMessage, RelayUrl, SubscriptionId, Timestamp};

use std::collections::BTreeSet;

/// A `RowSink` that just records every batch it is handed, for assertions.
#[derive(Clone, Default)]
struct CapturingSink(Arc<Mutex<Vec<Vec<RowDelta>>>>);

impl RowSink for CapturingSink {
    fn on_rows(&self, rows: Vec<RowDelta>) {
        self.0.lock().unwrap().push(rows);
    }
}

fn cf(kinds: &[u16], authors: &[&str]) -> ConcreteFilter {
    ConcreteFilter {
        kinds: Some(kinds.iter().copied().collect()),
        authors: Some(authors.iter().map(|s| s.to_string()).collect()),
        ..ConcreteFilter::default()
    }
}

fn literal_query(kinds: &[u16], author_hex: &str) -> LiveQuery {
    LiveQuery(Filter {
        kinds: Some(kinds.iter().copied().collect()),
        authors: Some(Binding::Literal(BTreeSet::from([author_hex.to_string()]))),
        ..Filter::default()
    })
}

fn new_core(dir: FixtureDirectory) -> EngineCore<MemoryStore> {
    EngineCore::new(MemoryStore::new(), Box::new(dir), 10)
}

/// Find the single `WireOp::Req` for `relay` inside `effects`, panicking if
/// there isn't exactly one (test-fixture convenience, not production code).
fn req_for<'a>(effects: &'a [Effect], relay: &RelayUrl) -> (&'a SubId, &'a ConcreteFilter) {
    for effect in effects {
        if let Effect::Wire(delta) = effect {
            for (r, ops) in &delta.ops {
                if r == relay {
                    for op in ops {
                        if let WireOp::Req(sub_id, filter) = op {
                            return (sub_id, filter);
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

// ---- test 1 analog: subscribe -> Wire; ingest -> Wire + EmitRows --------

#[test]
fn subscribe_opens_wire_for_resolved_demand() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);

    let effects = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));

    let (_sub_id, filter) = req_for(&effects, &relay0);
    assert_eq!(filter, &cf(&[1], &[&a.public_key().to_hex()]));
}

#[test]
fn ingest_frame_recompiles_wire_and_emits_rows() {
    let a = Keys::generate();
    let b = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new()
        .with_write(a.public_key().to_hex(), [relay0.clone()])
        .with_write(b.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);

    connect(&mut core, 0, &relay0);

    // $myFollows shape: kinds:[1], authors := Derived(inner=kind:3 by me,
    // project=#p) -- exactly nmp-resolver's M1 contract-test shape.
    let my_follows = LiveQuery(Filter {
        kinds: Some(BTreeSet::from([1u16])),
        authors: Some(Binding::Derived(Box::new(nmp_grammar::Derived {
            inner: Filter {
                kinds: Some(BTreeSet::from([3u16])),
                authors: Some(Binding::Reactive(nmp_grammar::IdentityField::ActivePubkey)),
                ..Filter::default()
            },
            project: nmp_grammar::Selector::Tag(nmp_grammar::TagName::new('p').unwrap()),
        }))),
        ..Filter::default()
    });

    let sink = CapturingSink::default();
    let _ = core.handle(EngineMsg::SetActivePubkey(Some(a.public_key())));
    let _ = core.handle(EngineMsg::Subscribe(my_follows, Box::new(sink.clone())));

    // B's kind:1 post arrives UNSOLICITED (before B is ever followed) --
    // the store holds it, but it matches no handle's root atoms yet.
    let b_post = nmp_resolver::testkit::kind1(&b, "hello from b", 50);
    let pre_effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        event_frame("s", b_post.clone()),
    ));
    assert!(
        !pre_effects
            .iter()
            .any(|e| matches!(e, Effect::EmitRows(_, rows, _) if !rows.is_empty())),
        "b's post must not be visible before b is followed"
    );

    // Now `a` follows `b`: root atoms fan out to include {kind:1,
    // authors:{b}} -- demand changes (Wire opens b's write relay) AND the
    // handle's row set changes (b's pre-existing post is now in scope).
    let contact_list = nmp_resolver::testkit::kind3(&a, &[b.public_key()], 100);
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        event_frame("s", contact_list),
    ));

    assert!(
        effects.iter().any(|e| matches!(e, Effect::Wire(_))),
        "ingest must recompile and open the new author's atom on the wire"
    );
    let emitted = effects.iter().find_map(|e| match e {
        Effect::EmitRows(_, rows, _) => Some(rows),
        _ => None,
    });
    let rows = emitted.expect("ingest must emit rows for the affected handle");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].event.id, b_post.id);

    // The sink was also called synchronously with the same rows.
    let captured = sink.0.lock().unwrap();
    assert!(captured
        .iter()
        .any(|batch| batch.len() == 1 && batch[0].event.id == b_post.id));
}

// ---- test 2 analog: EOSE records a watermark; a bare EVENT never does ---

#[test]
fn eose_records_coverage_watermark_and_non_eose_does_not() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);
    connect(&mut core, 0, &relay0);

    let atom = cf(&[3], &[&a.public_key().to_hex()]);
    let effects = core.handle(EngineMsg::Subscribe(
        literal_query(&[3], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let (sub_id, _filter) = req_for(&effects, &relay0);
    let wire = wire_sub_string(sub_id);

    // A bare EVENT frame (no EOSE yet) must record nothing.
    let e = nmp_resolver::testkit::kind3(&a, &[], 10);
    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        event_frame(&wire, e),
    ));
    assert_eq!(
        core.get_coverage(&atom, &relay0),
        None,
        "presence != coverage"
    );

    // The EOSE proves the (unfloored) window up to the engine clock.
    let _ = core.handle(EngineMsg::Tick(Timestamp::from(500u64)));
    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        eose_frame(&wire),
    ));

    let interval = core
        .get_coverage(&atom, &relay0)
        .expect("EOSE must record a coverage row");
    assert_eq!(interval.from, Timestamp::from(0u64));
    assert_eq!(interval.through, Timestamp::from(500u64));
}

// ---- the EOSE-overwrite-race rule (ruling §2) ---------------------------

#[test]
fn eose_overwrite_race_credits_only_the_intersection() {
    let a = Keys::generate();
    let e_key = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new()
        .with_write(a.public_key().to_hex(), [relay0.clone()])
        .with_write(e_key.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);
    connect(&mut core, 0, &relay0);

    // First subscribe: sends REQ(sub, {authors:{a}}) -- snapshot1 absorbs
    // {h_a} only.
    let effects1 = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let (sub_id, _f) = req_for(&effects1, &relay0);
    let sub_id = sub_id.clone();
    let wire = wire_sub_string(&sub_id);

    // Second subscribe (same skeleton, same relay): AuthorUnion widens the
    // SAME sub_id's filter to {a, e} -- an OVERWRITING REQ, snapshot2
    // absorbs {h_a, h_e}, pushed onto the SAME FIFO alongside snapshot1.
    let effects2 = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &e_key.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let (sub_id2, filter2) = req_for(&effects2, &relay0);
    assert_eq!(sub_id2, &sub_id, "same skeleton must reuse the sub id");
    assert_eq!(
        filter2.authors,
        Some(BTreeSet::from([
            a.public_key().to_hex(),
            e_key.public_key().to_hex()
        ]))
    );

    // A straggler EOSE for the sub now arrives, while BOTH snapshots are
    // outstanding.
    let _ = core.handle(EngineMsg::Tick(Timestamp::from(100u64)));
    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        eose_frame(&wire),
    ));

    let atom_a = cf(&[1], &[&a.public_key().to_hex()]);
    let atom_e = cf(&[1], &[&e_key.public_key().to_hex()]);
    assert!(
        core.get_coverage(&atom_a, &relay0).is_some(),
        "a is in BOTH outstanding snapshots -- must be credited"
    );
    assert!(
        core.get_coverage(&atom_e, &relay0).is_none(),
        "e is only in the newer snapshot -- the straggler EOSE must NOT credit it"
    );

    // The next EOSE (for the newer, still-outstanding snapshot) credits e.
    let _ = core.handle(EngineMsg::Tick(Timestamp::from(200u64)));
    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        eose_frame(&wire),
    ));
    assert!(
        core.get_coverage(&atom_e, &relay0).is_some(),
        "the second EOSE must credit the still-outstanding snapshot's atoms"
    );
}

// ---- limit poisons coverage ----------------------------------------------

#[test]
fn limited_fetch_never_records_coverage() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);
    connect(&mut core, 0, &relay0);

    let limited_query = LiveQuery(Filter {
        kinds: Some(BTreeSet::from([1u16])),
        authors: Some(Binding::Literal(BTreeSet::from([a.public_key().to_hex()]))),
        limit: Some(500),
        ..Filter::default()
    });
    let effects = core.handle(EngineMsg::Subscribe(
        limited_query,
        Box::new(CapturingSink::default()),
    ));
    let (sub_id, filter) = req_for(&effects, &relay0);
    assert_eq!(filter.limit, Some(500));
    let wire = wire_sub_string(sub_id);

    let _ = core.handle(EngineMsg::Tick(Timestamp::from(500u64)));
    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        eose_frame(&wire),
    ));

    let atom = cf(&[1], &[&a.public_key().to_hex()]);
    assert_eq!(
        core.get_coverage(&atom, &relay0),
        None,
        "a limited REQ's EOSE must poison -- never record a watermark"
    );
}

// ---- per-query CompleteUpTo aggregation (ruling §6, unanimity) ---------

#[test]
fn query_reads_complete_up_to_only_when_every_covering_relay_is_proven() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let relay1 = RelayUrl::parse("wss://relay1.example.com").unwrap();
    let dir = FixtureDirectory::new()
        .with_write(a.public_key().to_hex(), [relay0.clone(), relay1.clone()]);
    let mut core = new_core(dir);
    connect(&mut core, 0, &relay0);
    connect(&mut core, 1, &relay1);

    let sink = CapturingSink::default();
    let effects = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(sink.clone()),
    ));
    let (sub0, _) = req_for(&effects, &relay0);
    let (sub1, _) = req_for(&effects, &relay1);
    let wire0 = wire_sub_string(sub0);
    let wire1 = wire_sub_string(sub1);

    // Only relay0 finishes: the query must stay Unknown (relay1 -- possibly
    // the sole holder of some event -- hasn't proven anything yet). Nothing
    // OBSERVABLE changed for the handle (rows: still none; coverage: still
    // Unknown), so `EmitRows` correctly does not re-fire at all -- the
    // falsifiable claim is simply that it never reports `CompleteUpTo` here.
    let _ = core.handle(EngineMsg::Tick(Timestamp::from(10u64)));
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        eose_frame(&wire0),
    ));
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::EmitRows(_, _, QueryCoverage::CompleteUpTo(_)))),
        "one-of-two covering relays proven must NOT read as complete"
    );

    // relay1 also finishes: NOW the query is CompleteUpTo the min watermark.
    let _ = core.handle(EngineMsg::Tick(Timestamp::from(20u64)));
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 1,
            generation: 1,
        },
        eose_frame(&wire1),
    ));
    let coverage_after_both = effects.iter().find_map(|e| match e {
        Effect::EmitRows(_, _, cov) => Some(*cov),
        _ => None,
    });
    assert_eq!(
        coverage_after_both,
        Some(QueryCoverage::CompleteUpTo(Timestamp::from(10u64)))
    );
}

// ---- set-active-pubkey re-root ------------------------------------------

#[test]
fn set_active_pubkey_reroots_and_recompiles() {
    let a = Keys::generate();
    let b = Keys::generate();
    let relay_a = RelayUrl::parse("wss://relay-a.example.com").unwrap();
    let relay_b = RelayUrl::parse("wss://relay-b.example.com").unwrap();
    let dir = FixtureDirectory::new()
        .with_write(a.public_key().to_hex(), [relay_a.clone()])
        .with_write(b.public_key().to_hex(), [relay_b.clone()]);
    let mut core = new_core(dir);

    let whoami = LiveQuery(Filter {
        kinds: Some(BTreeSet::from([0u16])),
        authors: Some(Binding::Reactive(nmp_grammar::IdentityField::ActivePubkey)),
        ..Filter::default()
    });

    let _ = core.handle(EngineMsg::SetActivePubkey(Some(a.public_key())));
    let effects = core.handle(EngineMsg::Subscribe(
        whoami,
        Box::new(CapturingSink::default()),
    ));
    req_for(&effects, &relay_a); // demand is currently for `a`.

    let effects = core.handle(EngineMsg::SetActivePubkey(Some(b.public_key())));
    let closed_a = effects.iter().any(|e| {
        matches!(e, Effect::Wire(d) if d.ops.iter().any(|(r, ops)| r == &relay_a && ops.iter().any(|op| matches!(op, WireOp::Close(_)))))
    });
    assert!(closed_a, "re-root must close a's demand");
    req_for(&effects, &relay_b); // and open b's.
}
