//! #105 falsifiers: relay-provenance projection onto the reactive
//! `Row`/`RowDelta` surface. The union itself is nmp-store's job, already
//! exhaustively covered at that layer (`nmp-store/tests/store_contract.rs`'s
//! `provenance_merges_across_relays` etc.) -- these tests are scoped to the
//! NEW code this issue adds: `EngineCore::rows_and_evidence_for` projecting
//! `StoredEvent::provenance` instead of discarding it, and `refresh_handle`
//! detecting per-id provenance growth the same way it already detects
//! `AcquisitionEvidence` change (a plain value compare against remembered
//! state), never a second bespoke mechanism.

use std::collections::BTreeSet;

use nmp_engine::core::{Effect, EngineCore, EngineMsg, RowDelta};
use nmp_grammar::{Binding, Filter, RelaySessionKey};
use nmp_grammar::{Demand, LiveQuery};
use nmp_router_testkit::FixtureRoutingFacts;
use nmp_store::RedbStore;
use nmp_transport::{RelayFrame, RelayHandle};
use nostr::{Keys, RelayMessage, RelayUrl, SubscriptionId, Timestamp};

fn append_row_deltas(effects: &[Effect], delivered: &mut Vec<RowDelta>) {
    for effect in effects {
        if let Effect::EmitRows(_, deltas, _) = effect {
            delivered.extend(deltas.iter().cloned());
        }
    }
}

fn literal_kind_query(kind: u16, author_hex: &str) -> LiveQuery {
    LiveQuery::single(Demand {
        selection: Filter {
            kinds: Some(BTreeSet::from([kind])),
            authors: Some(Binding::Literal(BTreeSet::from([author_hex.to_string()]))),
            ..Filter::default()
        },
        ..Demand::default()
    })
}

fn connect(core: &mut EngineCore, slot: u32, url: &RelayUrl) {
    core.handle(EngineMsg::RelayConnected(
        RelayHandle {
            slot,
            generation: 1,
        },
        RelaySessionKey::public(url.clone()),
    ));
}

fn event_frame(sub: &str, event: nostr::Event) -> RelayFrame {
    RelayFrame::from(RelayMessage::event(SubscriptionId::new(sub), event))
}

fn deliver(
    core: &mut EngineCore,
    slot: u32,
    relay: &RelayUrl,
    event: &nostr::Event,
) -> Vec<Effect> {
    core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot,
            generation: 1,
        },
        RelaySessionKey::public(relay.clone()),
        event_frame("s", event.clone()),
    ))
}

#[test]
fn same_event_id_from_two_relays_unions_into_one_row_with_both_sources() {
    let author = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let relay1 = RelayUrl::parse("wss://relay1.example.com").unwrap();
    let dir = FixtureRoutingFacts::new()
        .with_outbound_routes(author.public_key(), [relay0.clone(), relay1.clone()]);
    let mut core = EngineCore::new_with_fixture_routing_facts(
        RedbStore::temporary().expect("temporary Redb store"),
        dir,
        10,
    );
    connect(&mut core, 0, &relay0);
    connect(&mut core, 1, &relay1);

    core.handle(EngineMsg::Subscribe(literal_kind_query(
        1,
        &author.public_key().to_hex(),
    )));
    let mut delivered = Vec::new();

    let event = nmp_resolver_testkit::kind1(&author, "provenance falsifier", 100);

    // Arrives from relay0 first: a brand-new row, sources == {relay0}.
    let effects = deliver(&mut core, 0, &relay0, &event);
    append_row_deltas(&effects, &mut delivered);
    let added = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitRows(_, rows, _) => rows.iter().find_map(|r| match r {
                RowDelta::Added(row) if row.id() == event.id => Some(row.sources.clone()),
                _ => None,
            }),
            _ => None,
        })
        .expect("relay0's delivery must be a fresh Added row");
    assert_eq!(added, BTreeSet::from([relay0.clone()]));
    assert_eq!(
        delivered
            .iter()
            .filter(|delta| matches!(delta, RowDelta::Added(row) if row.id() == event.id))
            .count(),
        1,
        "exactly one Added for this id so far"
    );
    assert!(
        !delivered
            .iter()
            .any(|delta| matches!(delta, RowDelta::SourcesGrew { id, .. } if *id == event.id)),
        "no SourcesGrew before a second relay has ever delivered it"
    );

    // The SAME event id, redelivered from relay0 again (identical
    // observation) -- the store-layer merge no-ops this; no delta at all,
    // and certainly no second Added or a spurious SourcesGrew.
    let effects = deliver(&mut core, 0, &relay0, &event);
    append_row_deltas(&effects, &mut delivered);
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::EmitRows(_, rows, _) if !rows.is_empty())),
        "an identical redelivery from an already-known relay must emit nothing"
    );
    assert_eq!(
        delivered
            .iter()
            .filter(|delta| matches!(delta, RowDelta::Added(row) if row.id() == event.id))
            .count(),
        1,
        "still exactly one Added"
    );
    assert!(!delivered
        .iter()
        .any(|delta| matches!(delta, RowDelta::SourcesGrew { id, .. } if *id == event.id)));

    // Now relay1 delivers the SAME event id: the row's provenance genuinely
    // grows. This must be `SourcesGrew`, never a second `Added` (that would
    // falsely claim the row "newly matches" a second time).
    let effects = deliver(&mut core, 1, &relay1, &event);
    append_row_deltas(&effects, &mut delivered);
    let grown = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitRows(_, rows, _) => rows.iter().find_map(|r| match r {
                RowDelta::SourcesGrew { id, sources } if *id == event.id => Some(sources.clone()),
                _ => None,
            }),
            _ => None,
        })
        .expect("relay1's delivery of the same id must emit SourcesGrew");
    assert_eq!(
        grown,
        BTreeSet::from([relay0.clone(), relay1.clone()]),
        "SourcesGrew must carry the FULL current source set, not just the new relay"
    );
    assert_eq!(
        delivered
            .iter()
            .filter(|delta| matches!(delta, RowDelta::Added(row) if row.id() == event.id))
            .count(),
        1,
        "still exactly one Added ever -- growth is never a second Added"
    );
    assert_eq!(
        delivered
            .iter()
            .filter(|delta| matches!(delta, RowDelta::SourcesGrew { id, .. } if *id == event.id))
            .count(),
        1
    );
}

/// The load-bearing falsifier (post-#77): `refresh_all_handles` now fires on
/// EVERY handle lifecycle event (any subscribe/unsubscribe recomputes AND
/// refreshes every surviving handle), not only on relay/wire activity. If
/// `SourcesGrew` suppression leaned on "the store-layer merge already
/// no-op'd" rather than on `HandleState.last_rows`'s own remembered
/// per-id source-set compare, a recompute triggered by some UNRELATED
/// query's lifecycle could spuriously re-emit `SourcesGrew` for a row whose
/// provenance never actually changed. It must not.
#[test]
fn unrelated_handle_lifecycle_never_spuriously_emits_sources_grew() {
    let author = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let relay1 = RelayUrl::parse("wss://relay1.example.com").unwrap();
    let dir = FixtureRoutingFacts::new()
        .with_outbound_routes(author.public_key(), [relay0.clone(), relay1.clone()]);
    let mut core = EngineCore::new_with_fixture_routing_facts(
        RedbStore::temporary().expect("temporary Redb store"),
        dir,
        10,
    );
    connect(&mut core, 0, &relay0);
    connect(&mut core, 1, &relay1);

    core.handle(EngineMsg::Subscribe(literal_kind_query(
        1,
        &author.public_key().to_hex(),
    )));
    let mut delivered = Vec::new();

    let event = nmp_resolver_testkit::kind1(&author, "lifecycle falsifier", 200);
    append_row_deltas(&deliver(&mut core, 0, &relay0, &event), &mut delivered);

    // An UNRELATED second query (different kind, matches nobody this store
    // holds) opens and closes -- this forces `refresh_all_handles`, which
    // recomputes and refreshes EVERY surviving handle, including the one
    // above, even though nothing about relay0/relay1/this event changed.
    let subscribe_effects = core.handle(EngineMsg::Subscribe(literal_kind_query(
        9999,
        "0000000000000000000000000000000000000000000000000000000000000000",
    )));
    let other_handle = subscribe_effects
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitRows(id, _, _) => Some(*id),
            _ => None,
        })
        .expect("the unrelated subscribe must itself emit at least one EmitRows (its own, empty, initial batch)");
    let unsubscribe_effects = core.handle(EngineMsg::Unsubscribe(other_handle));
    append_row_deltas(&subscribe_effects, &mut delivered);
    append_row_deltas(&unsubscribe_effects, &mut delivered);

    assert!(
        !delivered
            .iter()
            .any(|delta| matches!(delta, RowDelta::SourcesGrew { id, .. } if *id == event.id)),
        "an unrelated handle's own subscribe/unsubscribe lifecycle must never spuriously grow \
         this row's provenance -- the suppression must be keyed on this handle's own \
         remembered source set, not on whether the store-layer merge happened to no-op"
    );
    assert_eq!(
        delivered
            .iter()
            .filter(|delta| matches!(delta, RowDelta::Added(row) if row.id() == event.id))
            .count(),
        1,
        "still exactly one Added"
    );

    // Only a REAL second-relay observation may grow it, and it must still
    // do so correctly after all that unrelated lifecycle churn.
    append_row_deltas(&deliver(&mut core, 1, &relay1, &event), &mut delivered);
    let grown: Vec<_> = delivered
        .iter()
        .filter_map(|delta| match delta {
            RowDelta::SourcesGrew { id, sources } if *id == event.id => Some(sources.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        grown,
        vec![BTreeSet::from([relay0.clone(), relay1.clone()])],
        "exactly one SourcesGrew, with the full unioned set, once a real second relay delivers it"
    );
}

/// #105's persistence claim: the projected `sources` set survives a genuine
/// Redb close/reopen (the underlying union already does -- nmp-store's own
/// tests cover that exhaustively; this proves the NEW reducer-level
/// projection reads it back correctly too, through a fresh `EngineCore`
/// over the same store file).
#[test]
fn projected_sources_survive_a_real_redb_reopen() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("row-provenance.redb");
    let author = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let relay1 = RelayUrl::parse("wss://relay1.example.com").unwrap();
    let event = nmp_resolver_testkit::kind1(&author, "redb reopen falsifier", 300);

    {
        let mut store = RedbStore::open(&path).expect("redb: open");
        store
            .insert(
                event.clone(),
                nmp_store::RelayObserved::new(relay0.clone(), Timestamp::from(300)),
            )
            .unwrap();
        store
            .insert(
                event.clone(),
                nmp_store::RelayObserved::new(relay1.clone(), Timestamp::from(301)),
            )
            .unwrap();
    }

    let store = RedbStore::open(&path).expect("redb: reopen");
    let dir = FixtureRoutingFacts::new()
        .with_outbound_routes(author.public_key(), [relay0.clone(), relay1.clone()]);
    let mut core = EngineCore::new_with_fixture_routing_facts(store, dir, 10);
    assert!(core.recover_on_boot().is_empty());

    let effects = core.handle(EngineMsg::Subscribe(literal_kind_query(
        1,
        &author.public_key().to_hex(),
    )));
    let sources = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitRows(_, rows, _) => rows.iter().find_map(|r| match r {
                RowDelta::Added(row) if row.id() == event.id => Some(row.sources.clone()),
                _ => None,
            }),
            _ => None,
        })
        .expect("the reopened store's persisted union must still project as this row's sources");
    assert_eq!(sources, BTreeSet::from([relay0, relay1]));
}
