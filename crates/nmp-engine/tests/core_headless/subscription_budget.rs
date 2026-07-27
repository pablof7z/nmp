//! The NIP-11 hop (#931): a relay's advertised `limitation.max_subscriptions`
//! travelling document → capability evidence → `EngineCore::compile_budget`
//! → the plan, and the refresh behaviour that hop implies.
//!
//! `nmp-router`'s own `tests/subscription_budget.rs` proves the ENFORCEMENT
//! against a synthetic budget. What can only be proven here is the wiring
//! either side of it: that an advertisement resolved AFTER the compile that
//! planned the relay still binds (NIP-11 acquisition is driven by connect,
//! so it always arrives late), that a re-published document replans, and that
//! ordinary document churn does not.
//!
//! Run narrated with:
//! `cargo test -p nmp-engine --test core_headless subscription_budget -- --nocapture`

use super::*;

fn relay() -> RelayUrl {
    RelayUrl::parse("wss://budgeted.example.com").unwrap()
}

/// One pinned, LIMITED demand. A `limit` caps the result count rather than
/// the predicate, so two of these never merge — the honest way to hold
/// several concurrent subscriptions open against one relay.
fn limited_pinned_query(kind: u16) -> LiveQuery {
    LiveQuery(
        nmp_grammar::Demand::new(
            Filter {
                kinds: Some(BTreeSet::from([kind])),
                limit: Some(10),
                ..Filter::default()
            },
            SourceAuthority::Pinned(BTreeSet::from([relay()])),
            AccessContext::Public,
        )
        .expect("pinned demand with a nonempty relay set is constructible"),
    )
}

/// A NIP-11 document advertising exactly these limits.
fn advertising(
    max_subscriptions: Option<u64>,
    max_subid_length: Option<u64>,
) -> nmp_engine::relay_information::RelayInformationCapabilityEvidence {
    nmp_engine::relay_information::RelayInformationCapabilityEvidence {
        supported_nips: Some(vec![11]),
        max_subscriptions,
        max_subid_length,
        document_revision: format!("revision-{max_subscriptions:?}-{max_subid_length:?}"),
        fresh_until: u64::MAX,
        last_error: None,
    }
}

/// The set of subscriptions currently LIVE on the wire, folded from the
/// `WireOp`s the reducer actually emitted — `Req` opens or replaces, `Close`
/// withdraws. Deliberately not read out of any engine-internal plan: what a
/// relay is holding open is what was sent to it.
#[derive(Default)]
struct LiveWire(BTreeSet<SubId>);

impl LiveWire {
    fn apply(&mut self, effects: &[Effect]) {
        for effect in effects {
            let Effect::Wire(delta) = effect else {
                continue;
            };
            for (_, ops) in &delta.ops {
                for op in ops {
                    match op {
                        WireOp::Req(sub_id, _) => {
                            self.0.insert(sub_id.clone());
                        }
                        WireOp::Close(sub_id) => {
                            self.0.remove(sub_id);
                        }
                    }
                }
            }
        }
    }

    fn ids(&self) -> BTreeSet<SubId> {
        self.0.clone()
    }
}

/// Open `count` mutually unmergeable watches, then connect the relay without
/// resolving any document.
fn core_watching(count: u16) -> (EngineCore<MemoryStore>, LiveWire) {
    let mut core = new_core(FixtureDirectory::new());
    let mut wire = LiveWire::default();
    for index in 0..count {
        let effects = core.handle(EngineMsg::Subscribe(
            limited_pinned_query(30_000 + index),
            Box::new(CapturingSink::default()),
        ));
        wire.apply(&effects);
    }
    let effects = core.handle(EngineMsg::RelayConnected(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay()),
    ));
    wire.apply(&effects);
    (core, wire)
}

fn relay_row(core: &EngineCore<MemoryStore>) -> nmp_engine::core::RelayDiagnosticsSnapshot {
    core.diagnostics_snapshot()
        .relays
        .into_iter()
        .find(|row| row.relay == relay())
        .expect("the planned relay must be diagnosable")
}

/// A document arriving AFTER the compile that planned the relay still binds.
///
/// This is the ordering the engine actually has — `Effect::FetchRelayInformation`
/// is emitted on connect, and connect happens after planning — so an
/// advertisement that only took effect at the next unrelated demand mutation
/// would be advisory wearing an enforced label.
#[test]
fn an_advertisement_resolved_after_planning_binds_immediately() {
    let (mut core, _wire) = core_watching(4);
    assert_eq!(relay_row(&core).wire_sub_count, 4);
    assert_eq!(relay_row(&core).subscription_budget, None);

    let effects = core.handle(EngineMsg::RelayInformationResolved(
        relay(),
        Some(advertising(Some(2), None)),
    ));

    let row = relay_row(&core);
    assert_eq!(row.subscription_budget, Some(2));
    assert_eq!(row.wire_sub_count, 2, "the advertised budget must bind");
    assert_eq!(row.subscriptions_refused, 2);
    assert!(
        effects.iter().any(|effect| matches!(
            effect,
            Effect::Wire(delta) if delta
                .ops
                .iter()
                .any(|(_, ops)| ops.iter().any(|op| matches!(op, WireOp::Close(_))))
        )),
        "learning the budget must withdraw the subscriptions it refuses, \
         not leave them open: {effects:?}"
    );
}

/// The app is TOLD. Every refused subscription's demand carries
/// `ShortfallFact::LocalLimit` in its own acquisition evidence — the same
/// seam the whole-demand relay ceiling has always used. Silent truncation is
/// the one outcome a budget must never be.
#[test]
fn refused_demand_reaches_the_app_as_an_explicit_local_limit() {
    let (mut core, _wire) = core_watching(4);
    let effects = core.handle(EngineMsg::RelayInformationResolved(
        relay(),
        Some(advertising(Some(2), None)),
    ));

    let limited: Vec<&AcquisitionEvidence> = effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::EmitRows(_, _, evidence) => Some(evidence),
            _ => None,
        })
        .filter(|evidence| {
            evidence
                .shortfall
                .iter()
                .any(|fact| matches!(fact, ShortfallFact::LocalLimit { .. }))
        })
        .collect();
    assert_eq!(
        limited.len(),
        2,
        "exactly the two refused watches must report a local limit: {effects:?}"
    );
}

/// A relay that advertises no `max_subscriptions` is UNBUDGETED. Two of the
/// eight relays measured for this issue publish no NIP-11 document at all;
/// fabricating a number for them would drop demand they never refused.
#[test]
fn a_relay_advertising_nothing_keeps_every_subscription() {
    let (mut core, _wire) = core_watching(4);

    let _ = core.handle(EngineMsg::RelayInformationResolved(relay(), None));
    let row = relay_row(&core);
    assert_eq!(row.wire_sub_count, 4);
    assert_eq!(row.subscription_budget, None);
    assert_eq!(row.subscriptions_refused, 0);

    // A document that exists but says nothing about subscriptions is
    // exactly as unbudgeted -- a present `limitation` object with an absent
    // field must not collapse into a number either.
    let _ = core.handle(EngineMsg::RelayInformationResolved(
        relay(),
        Some(advertising(None, Some(100))),
    ));
    let row = relay_row(&core);
    assert_eq!(row.wire_sub_count, 4);
    assert_eq!(row.subscription_budget, None);
}

/// A re-published document replans — and relaxing the budget re-admits the
/// refused demand without renaming what was already being served.
#[test]
fn a_republished_document_replans_without_disturbing_incumbents() {
    let (mut core, mut wire) = core_watching(4);
    let effects = core.handle(EngineMsg::RelayInformationResolved(
        relay(),
        Some(advertising(Some(2), None)),
    ));
    wire.apply(&effects);
    let incumbents = wire.ids();
    assert_eq!(incumbents.len(), 2);

    let effects = core.handle(EngineMsg::RelayInformationResolved(
        relay(),
        Some(advertising(Some(200), None)),
    ));
    wire.apply(&effects);

    let row = relay_row(&core);
    assert_eq!(row.wire_sub_count, 4, "a relaxed budget re-admits");
    assert_eq!(row.subscriptions_refused, 0);
    assert!(
        wire.ids().is_superset(&incumbents),
        "a refreshed document must not rename an established subscription"
    );
}

/// Ordinary document churn is not a replan. Only the numbers the PLANNER
/// reads may cost a recompile; a changed NIP list or revision must not.
#[test]
fn document_churn_that_does_not_move_the_budget_does_not_replan() {
    let (mut core, mut wire) = core_watching(4);
    let mut settled = advertising(Some(2), None);
    let effects = core.handle(EngineMsg::RelayInformationResolved(
        relay(),
        Some(settled.clone()),
    ));
    wire.apply(&effects);
    let before = wire.ids();

    settled.supported_nips = Some(vec![11, 42, 50]);
    settled.document_revision = "a-later-revision".to_string();
    let effects = core.handle(EngineMsg::RelayInformationResolved(relay(), Some(settled)));

    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::Wire(_))),
        "a document whose limits did not move must not touch the wire: {effects:?}"
    );
    assert_eq!(wire.ids(), before);
}

/// `max_subid_length` is diagnosed and NOTHING else. NMP's wire ids are
/// fixed 64-character strings; a relay advertising less rejects every REQ,
/// which is worth saying out loud — and must never become an input to id
/// derivation, because this document refreshes.
#[test]
fn an_advertised_subscription_id_length_below_ours_is_diagnosed_only() {
    let (mut core, mut wire) = core_watching(2);
    let before = wire.ids();

    let effects = core.handle(EngineMsg::RelayInformationResolved(
        relay(),
        Some(advertising(None, Some(32))),
    ));
    wire.apply(&effects);

    let row = relay_row(&core);
    assert_eq!(row.subid_length_limit, Some(32));
    assert!(row.subid_length_rejects_our_ids);
    assert_eq!(row.wire_sub_count, 2, "a short id limit refuses nothing");
    assert_eq!(wire.ids(), before, "and moves no established wire id");

    // nostr.wine advertises 71; 64 is NIP-01's own cap. Both fit.
    let effects = core.handle(EngineMsg::RelayInformationResolved(
        relay(),
        Some(advertising(None, Some(71))),
    ));
    wire.apply(&effects);
    let row = relay_row(&core);
    assert_eq!(row.subid_length_limit, Some(71));
    assert!(!row.subid_length_rejects_our_ids);
    assert_eq!(wire.ids(), before);
}
