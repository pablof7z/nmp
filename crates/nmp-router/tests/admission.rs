//! Pending-only relay-plan admission falsifiers for #1341.

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::{
    AccessContext, ConcreteFilter, ContextualAtom, IndexedTagName, RelaySessionKey, SourceAuthority,
};
use nmp_router::{FixtureRoutingFacts, Router, RuleRegistry, WireOp};
use nostr::{Keys, PublicKey, RelayUrl};

fn atom(relay: &RelayUrl, value: &str) -> ContextualAtom {
    atom_on(BTreeSet::from([relay.clone()]), value)
}

fn atom_on(relays: BTreeSet<RelayUrl>, value: &str) -> ContextualAtom {
    ContextualAtom {
        filter: ConcreteFilter {
            kinds: Some(BTreeSet::from([0u16])),
            tags: BTreeMap::from([(
                IndexedTagName::new('p').unwrap(),
                BTreeSet::from([value.to_owned()]),
            )]),
            ..ConcreteFilter::default()
        },
        source: SourceAuthority::Pinned(relays),
        access: AccessContext::Public,
        routing_evidence: BTreeSet::new(),
    }
}

fn routeless_outbox_atom(author: PublicKey) -> ContextualAtom {
    ContextualAtom {
        filter: ConcreteFilter {
            kinds: Some(BTreeSet::from([1u16])),
            authors: Some(BTreeSet::from([author.to_hex()])),
            ..ConcreteFilter::default()
        },
        source: SourceAuthority::AuthorOutboxes,
        access: AccessContext::Public,
        routing_evidence: BTreeSet::new(),
    }
}

fn reqs(delta: &nmp_router::WireDelta) -> usize {
    delta
        .ops
        .iter()
        .flat_map(|(_, ops)| ops)
        .filter(|op| matches!(op, WireOp::Req(_, _)))
        .count()
}

fn withdraw(
    router: &mut Router,
    atoms: impl IntoIterator<Item = ContextualAtom>,
    cap: usize,
) -> nmp_router::WireDelta {
    router.withdraw(atoms, cap).wire
}

#[test]
fn one_pending_cohort_coalesces_but_a_later_cohort_never_rewrites_it() {
    let relay = RelayUrl::parse("wss://router-admission.example").unwrap();
    let facts = FixtureRoutingFacts::new();
    let mut router = Router::new(RuleRegistry::default_widen_only());

    let first = router.admit(
        &BTreeSet::from([atom(&relay, "alice"), atom(&relay, "bob")]),
        &facts,
        20,
    );
    assert_eq!(reqs(&first), 1);
    let session = RelaySessionKey::public(relay.clone());
    let incumbent = router.plan().reqs[&session][0].clone();

    let later = router.admit(&BTreeSet::from([atom(&relay, "carol")]), &facts, 20);
    assert_eq!(reqs(&later), 1);
    assert!(later
        .ops
        .iter()
        .flat_map(|(_, ops)| ops)
        .all(|op| !matches!(op, WireOp::Close(_))));
    assert!(router.plan().reqs[&session]
        .iter()
        .any(|request| request == &incumbent));
    assert_eq!(router.plan().reqs[&session].len(), 2);
}

#[test]
fn exact_running_coverage_makes_repeated_admission_a_noop() {
    let relay = RelayUrl::parse("wss://router-covered.example").unwrap();
    let facts = FixtureRoutingFacts::new();
    let mut router = Router::new(RuleRegistry::default_widen_only());
    let demand = BTreeSet::from([atom(&relay, "alice")]);
    assert_eq!(reqs(&router.admit(&demand, &facts, 20)), 1);

    let duplicate = router.admit(&demand, &facts, 20);

    assert!(duplicate.ops.is_empty());
    assert_eq!(router.plan().reqs.len(), 1);
    assert_eq!(router.plan().reqs.values().flatten().count(), 1);
}

#[test]
fn a_live_request_does_not_absorb_a_later_windowed_backfill() {
    let relay = RelayUrl::parse("wss://router-window.example").unwrap();
    let facts = FixtureRoutingFacts::new();
    let mut router = Router::new(RuleRegistry::default_widen_only());
    let live = atom(&relay, "alice");
    let mut older = live.clone();
    older.filter.until = Some(99);
    older.filter.limit = Some(5);

    assert_eq!(
        reqs(&router.admit(&BTreeSet::from([live.clone()]), &facts, 20)),
        1
    );
    let backfill = router.admit(&BTreeSet::from([older.clone()]), &facts, 20);
    assert_eq!(reqs(&backfill), 1);
    assert!(backfill
        .ops
        .iter()
        .flat_map(|(_, ops)| ops)
        .all(|op| !matches!(op, WireOp::Close(_))));
    assert_eq!(router.plan().reqs.values().flatten().count(), 2);

    let retired = withdraw(&mut router, [older], 20);
    assert_eq!(
        retired
            .ops
            .iter()
            .flat_map(|(_, ops)| ops)
            .filter(|op| matches!(op, WireOp::Close(_)))
            .count(),
        1
    );
    assert_eq!(router.plan().reqs.values().flatten().count(), 1);
}

#[test]
fn withdrawal_keeps_a_shared_immutable_req_until_its_last_key_leaves() {
    let relay = RelayUrl::parse("wss://router-withdraw.example").unwrap();
    let facts = FixtureRoutingFacts::new();
    let mut router = Router::new(RuleRegistry::default_widen_only());
    let alice = atom(&relay, "alice");
    let bob = atom(&relay, "bob");
    router.admit(&BTreeSet::from([alice.clone(), bob.clone()]), &facts, 20);

    router.reset_withdrawal_work();
    let keep_bob = withdraw(&mut router, [alice.clone()], 20);
    assert!(keep_bob.ops.is_empty());
    assert_eq!(router.plan().reqs.values().flatten().count(), 1);
    assert_eq!(router.withdrawal_work().dropped_atoms, 1);
    assert_eq!(router.withdrawal_work().request_edges_touched, 1);
    assert_eq!(router.withdrawal_work().requests_closed, 0);
    assert_eq!(router.withdrawal_work().diagnostic_rebuilds, 0);

    let reattached = router.admit(&BTreeSet::from([alice.clone()]), &facts, 20);
    assert!(
        reattached.ops.is_empty(),
        "immutable physical coverage must be reusable without REQ"
    );

    let _ = withdraw(&mut router, [alice], 20);
    let close = withdraw(&mut router, [bob], 20);
    assert_eq!(
        close
            .ops
            .iter()
            .flat_map(|(_, ops)| ops)
            .filter(|op| matches!(op, WireOp::Close(_)))
            .count(),
        1
    );
    assert!(router.plan().reqs.is_empty());
}

#[test]
fn withdrawing_the_final_routeless_outbox_owner_retracts_its_diagnostic() {
    let facts = FixtureRoutingFacts::new();
    let mut router = Router::new(RuleRegistry::default_widen_only());
    let author = Keys::generate().public_key();
    let demand = routeless_outbox_atom(author);

    let admitted = router.admit(&BTreeSet::from([demand.clone()]), &facts, 20);
    assert!(admitted.ops.is_empty());
    assert!(router.diagnostics().uncovered_authors.contains_key(&author));

    router.reset_withdrawal_work();
    let withdrawn = router.withdraw([demand], 20);

    assert!(withdrawn.wire.ops.is_empty());
    assert!(withdrawn.changed_coverage.is_empty());
    assert!(withdrawn.diagnostics_changed);
    assert!(!router.diagnostics().uncovered_authors.contains_key(&author));
    assert_eq!(router.withdrawal_work().request_edges_touched, 0);
    assert_eq!(router.withdrawal_work().requests_closed, 0);
    assert_eq!(router.withdrawal_work().diagnostic_rebuilds, 1);
}

#[test]
fn a_refused_pending_atom_is_admitted_after_an_incumbent_releases_the_relay_cap() {
    let first_relay = RelayUrl::parse("wss://router-cap-first.example").unwrap();
    let second_relay = RelayUrl::parse("wss://router-cap-second.example").unwrap();
    let facts = FixtureRoutingFacts::new();
    let mut router = Router::new(RuleRegistry::default_widen_only());
    let first = atom(&first_relay, "alice");
    let second = atom(&second_relay, "bob");

    assert_eq!(
        reqs(&router.admit(&BTreeSet::from([first.clone()]), &facts, 1)),
        1
    );
    assert_eq!(
        reqs(&router.admit(&BTreeSet::from([second.clone()]), &facts, 1)),
        0
    );
    assert_eq!(router.plan().limited.len(), 1);

    let close = withdraw(&mut router, [first], 1);
    assert_eq!(
        close
            .ops
            .iter()
            .flat_map(|(_, ops)| ops)
            .filter(|op| matches!(op, WireOp::Close(_)))
            .count(),
        1
    );
    let admitted = router.admit(&BTreeSet::from([second]), &facts, 1);
    assert_eq!(reqs(&admitted), 1);
    assert!(router.plan().limited.is_empty());
    assert!(router.plan().refused_sessions.is_empty());
}

#[test]
fn ten_thousand_shared_keys_do_only_delta_edges_plus_one_physical_close() {
    let relay = RelayUrl::parse("wss://router-10k-withdraw.example").unwrap();
    let facts = FixtureRoutingFacts::new();
    let mut router = Router::new(RuleRegistry::default_widen_only());
    let demand: BTreeSet<_> = (0..10_000)
        .map(|index| atom(&relay, &format!("owner-{index:05}")))
        .collect();
    let physical_requests = reqs(&router.admit(&demand, &facts, 20));
    assert!(
        physical_requests < 50,
        "the 10k atoms should remain coalesced"
    );

    router.reset_withdrawal_work();
    let mut close_count = 0;
    for atom in demand {
        close_count += withdraw(&mut router, [atom], 20)
            .ops
            .iter()
            .flat_map(|(_, ops)| ops)
            .filter(|op| matches!(op, WireOp::Close(_)))
            .count();
    }

    let work = router.withdrawal_work();
    assert_eq!(close_count, physical_requests);
    assert_eq!(work.dropped_atoms, 10_000);
    assert_eq!(work.request_edges_touched, 10_000);
    assert_eq!(work.requests_closed, physical_requests as u64);
    assert_eq!(work.physical_coverage_edges_released, 10_000);
    assert_eq!(work.diagnostic_rebuilds, physical_requests as u64);
}

#[test]
fn lifting_a_partial_source_limit_adds_only_the_missing_session() {
    let first_relay = RelayUrl::parse("wss://router-partial-first.example").unwrap();
    let second_relay = RelayUrl::parse("wss://router-partial-second.example").unwrap();
    let facts = FixtureRoutingFacts::new();
    let mut router = Router::new(RuleRegistry::default_widen_only());
    let demand = BTreeSet::from([atom_on(
        BTreeSet::from([first_relay.clone(), second_relay.clone()]),
        "alice",
    )]);

    assert_eq!(reqs(&router.admit(&demand, &facts, 1)), 1);
    assert_eq!(router.plan().reqs.len(), 1);
    assert_eq!(router.plan().limited.len(), 1);
    let incumbent_session = router.plan().reqs.keys().next().unwrap().clone();
    let incumbent = router.plan().reqs[&incumbent_session][0].clone();

    let completed = router.admit(&demand, &facts, 2);

    assert_eq!(reqs(&completed), 1);
    assert_eq!(router.plan().reqs.len(), 2);
    assert_eq!(router.plan().reqs[&incumbent_session], vec![incumbent]);
    assert!(router.plan().limited.is_empty());
}
