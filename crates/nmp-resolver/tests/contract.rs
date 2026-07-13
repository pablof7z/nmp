//! The M1 contract tests (plan §5) — the pass criteria for the resolver.
//! Each test drives the REAL path through [`Harness`]: build event ->
//! `deliver` -> insert/supersede -> re-eval -> assert delta + metrics.
//!
//! Test 10 (the structural no-kind-branch guard) lives in
//! `tests/no_kind_branches.rs`.

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::{
    AccessContext, Binding, ConcreteFilter, ContextualAtom, Demand, DemandOp, Derived, Filter,
    IdentityField, IndexedTagName, Selector, SetAlgebra, SetOp, SourceAuthority,
};
use nmp_resolver::testkit::{
    addressable, deletion, kind10000_mutes, kind10003_bookmarks, kind3, kind39002, Harness,
};
use nmp_resolver::LiveQuery;
use nostr::{EventBuilder, Keys, Kind};

// ---- ConcreteFilter builders (test-local; mirrors nmp-grammar's own test
// helpers, kept separate since these assert resolver *output*, not grammar
// lowering) --------------------------------------------------------------

fn cf_kinds_authors(kinds: &[u16], authors: &[&str]) -> ConcreteFilter {
    ConcreteFilter {
        kinds: Some(kinds.iter().copied().collect()),
        authors: Some(authors.iter().map(|s| s.to_string()).collect()),
        ..ConcreteFilter::default()
    }
}

fn cf_kinds_tag(kinds: &[u16], tag: char, values: &[&str]) -> ConcreteFilter {
    let mut tags = BTreeMap::new();
    tags.insert(
        IndexedTagName::new(tag).unwrap(),
        values.iter().map(|s| s.to_string()).collect(),
    );
    ConcreteFilter {
        kinds: Some(kinds.iter().copied().collect()),
        tags,
        ..ConcreteFilter::default()
    }
}

fn cf_coord(kind: u16, author: &str, d: &str) -> ConcreteFilter {
    let mut tags = BTreeMap::new();
    tags.insert(
        IndexedTagName::new('d').unwrap(),
        BTreeSet::from([d.to_string()]),
    );
    ConcreteFilter {
        kinds: Some(BTreeSet::from([kind])),
        authors: Some(BTreeSet::from([author.to_string()])),
        tags,
        ..ConcreteFilter::default()
    }
}

/// Wrap a filter into a `ContextualAtom` under `source` (#106: `DemandOp`
/// carries the full atom now, not a bare `ConcreteFilter` -- Fable's
/// ratified shape). `access` is always `Public` in these tests.
fn atom(filter: ConcreteFilter, source: SourceAuthority) -> ContextualAtom {
    ContextualAtom {
        filter,
        source,
        access: AccessContext::Public,
    }
}

/// `Demand::from_filter`'s static default for any filter whose root FilterNode
/// binds `authors` at all (my_follows/follows_minus_mutes/address_coord's
/// root atoms) -- `AuthorOutboxes`.
fn outbox_atom(filter: ConcreteFilter) -> ContextualAtom {
    atom(filter, SourceAuthority::AuthorOutboxes)
}

/// `Demand::from_filter`'s static default for a filter whose root FilterNode
/// does NOT bind `authors` at all (nip29_groups/bookmarks' root and outer
/// atoms, which bind only tags) -- `Public`.
fn public_atom(filter: ConcreteFilter) -> ContextualAtom {
    atom(filter, SourceAuthority::Public)
}

// ---- LiveQuery shape builders --------------------------------------------

/// `kinds:[1], authors := Derived(inner=(kinds:[3], authors:[Reactive]),
/// project=Tag(p))` — "my follows" (tests 1, 3, 4, 5, 6, 8).
fn my_follows_filter() -> Filter {
    Filter {
        kinds: Some(BTreeSet::from([1u16])),
        authors: Some(Binding::Derived(Box::new(Derived {
            inner: Demand::from_filter(Filter {
                kinds: Some(BTreeSet::from([3u16])),
                authors: Some(Binding::Reactive(IdentityField::ActivePubkey)),
                ..Filter::default()
            }),
            project: Selector::Tag("p".to_string()),
        }))),
        ..Filter::default()
    }
}

/// `kinds:[39000,39001,39002], #d := Derived(inner=(kinds:[39002],
/// #p:[Reactive]), project=Tag(d))` — NIP-29-shaped groups (tests 2, 7).
fn nip29_groups_filter() -> Filter {
    let mut tags = BTreeMap::new();
    tags.insert(
        IndexedTagName::new('d').unwrap(),
        Binding::Derived(Box::new(Derived {
            inner: Demand::from_filter(Filter {
                kinds: Some(BTreeSet::from([39_002u16])),
                tags: {
                    let mut inner_tags = BTreeMap::new();
                    inner_tags.insert(
                        IndexedTagName::new('p').unwrap(),
                        Binding::Reactive(IdentityField::ActivePubkey),
                    );
                    inner_tags
                },
                ..Filter::default()
            }),
            project: Selector::Tag("d".to_string()),
        })),
    );
    Filter {
        kinds: Some(BTreeSet::from([39_000u16, 39_001u16, 39_002u16])),
        tags,
        ..Filter::default()
    }
}

/// `kinds:[1], authors := SetOp(Diff, [Derived(follows), Derived(mutes)])`
/// — "follows minus mutes" (test 9, amendment #1).
fn follows_minus_mutes_filter() -> Filter {
    let follows = Binding::Derived(Box::new(Derived {
        inner: Demand::from_filter(Filter {
            kinds: Some(BTreeSet::from([3u16])),
            authors: Some(Binding::Reactive(IdentityField::ActivePubkey)),
            ..Filter::default()
        }),
        project: Selector::Tag("p".to_string()),
    }));
    let mutes = Binding::Derived(Box::new(Derived {
        inner: Demand::from_filter(Filter {
            kinds: Some(BTreeSet::from([10_000u16])),
            authors: Some(Binding::Reactive(IdentityField::ActivePubkey)),
            ..Filter::default()
        }),
        project: Selector::Tag("p".to_string()),
    }));
    Filter {
        kinds: Some(BTreeSet::from([1u16])),
        authors: Some(Binding::SetOp(Box::new(SetOp {
            op: SetAlgebra::Diff,
            operands: vec![follows, mutes],
        }))),
        ..Filter::default()
    }
}

/// `authors := Derived(inner=(kinds:[30003], authors:[Reactive]),
/// project=AddressCoord)` — the co-pinned coordinate case (test 11,
/// amendment #3). The binding is attached to the `authors` slot
/// arbitrarily: an `AddressCoord` projection overrides
/// kinds/authors/`#d` together regardless of which grammar field carries
/// it (M1 plan §3.5).
fn address_coord_filter() -> Filter {
    Filter {
        authors: Some(Binding::Derived(Box::new(Derived {
            inner: Demand::from_filter(Filter {
                kinds: Some(BTreeSet::from([30_003u16])),
                authors: Some(Binding::Reactive(IdentityField::ActivePubkey)),
                ..Filter::default()
            }),
            project: Selector::AddressCoord,
        }))),
        ..Filter::default()
    }
}

/// `kinds:[1], #e := Derived(inner=(kinds:[10003], authors:[Reactive]),
/// project=Tag(e))` — a third, unrelated depth-1 shape (test 12, the
/// generality witness): bookmarks' `e`-tags. Built and passed with zero
/// changes to `nmp-resolver/src` beyond what tests 1-2 needed.
fn bookmarks_filter() -> Filter {
    let mut tags = BTreeMap::new();
    tags.insert(
        IndexedTagName::new('e').unwrap(),
        Binding::Derived(Box::new(Derived {
            inner: Demand::from_filter(Filter {
                kinds: Some(BTreeSet::from([10_003u16])),
                authors: Some(Binding::Reactive(IdentityField::ActivePubkey)),
                ..Filter::default()
            }),
            project: Selector::Tag("e".to_string()),
        })),
    );
    Filter {
        kinds: Some(BTreeSet::from([1u16])),
        tags,
        ..Filter::default()
    }
}

/// `kinds:[7], authors := Derived(inner=(kinds:[1], limit:N),
/// project=Authors)` — a generic bounded interior projection. The limit
/// selects events before their authors are projected.
fn bounded_recent_authors_filter(limit: usize) -> Filter {
    Filter {
        kinds: Some(BTreeSet::from([7u16])),
        authors: Some(Binding::Derived(Box::new(Derived {
            inner: Demand::from_filter(Filter {
                kinds: Some(BTreeSet::from([1u16])),
                limit: Some(limit),
                ..Filter::default()
            }),
            project: Selector::Authors,
        }))),
        ..Filter::default()
    }
}

fn dummy_event_id(seed: &str) -> nostr::EventId {
    let keys = Keys::generate();
    EventBuilder::new(Kind::TextNote, seed)
        .sign_with_keys(&keys)
        .unwrap()
        .id
}

#[test]
fn derived_inner_limit_selects_newest_events_and_refills_after_retraction() {
    use nmp_resolver::testkit::{deletion, kind1};

    let mut h = Harness::new();
    let a = Keys::generate();
    let b = Keys::generate();
    let c = Keys::generate();
    let d = Keys::generate();
    let (_handle, _open_delta) =
        h.subscribe(LiveQuery::from_filter(bounded_recent_authors_filter(2)));

    let event_a = kind1(&a, "oldest", 100);
    let event_b = kind1(&b, "middle", 200);
    let event_c = kind1(&c, "newest", 300);
    h.deliver(vec![event_a, event_b, event_c]);

    let inner = ConcreteFilter {
        kinds: Some(BTreeSet::from([1u16])),
        limit: Some(2),
        ..ConcreteFilter::default()
    };
    let outer_b = cf_kinds_authors(&[7], &[&b.public_key().to_hex()]);
    let outer_c = cf_kinds_authors(&[7], &[&c.public_key().to_hex()]);
    assert_eq!(
        h.demand(),
        BTreeSet::from([inner.clone(), outer_b.clone(), outer_c.clone()]),
        "the older third event must not influence a limit:2 Derived set"
    );

    let event_d = kind1(&d, "new top", 400);
    let event_d_id = event_d.id;
    let delta = h.deliver(vec![event_d]);
    let outer_d = cf_kinds_authors(&[7], &[&d.public_key().to_hex()]);
    assert_eq!(
        delta.ops,
        vec![
            DemandOp::Close(outbox_atom(outer_b.clone())),
            DemandOp::Open(outbox_atom(outer_d.clone()))
        ],
        "a newer event evicts exactly the previous top-N floor"
    );
    assert_eq!(
        h.demand(),
        BTreeSet::from([inner.clone(), outer_c.clone(), outer_d.clone()])
    );

    let delta = h.deliver(vec![deletion(&d, &[event_d_id], 500)]);
    assert_eq!(
        delta.ops,
        vec![
            DemandOp::Close(outbox_atom(outer_d)),
            DemandOp::Open(outbox_atom(outer_b.clone()))
        ],
        "retracting a top-N row pulls the next-newest event back in"
    );
    assert_eq!(h.demand(), BTreeSet::from([inner, outer_b, outer_c]));
}

// ---- 1. depth1_myfollows_surgical_delta ---------------------------------

#[test]
fn depth1_myfollows_surgical_delta() {
    let mut h = Harness::new();
    let a = Keys::generate();
    let b = Keys::generate();
    let c = Keys::generate();
    let d = Keys::generate();

    h.set_active(Some(a.public_key()));
    let (_handle, _open_delta) = h.subscribe(LiveQuery::from_filter(my_follows_filter()));
    h.deliver(vec![kind3(
        &a,
        &[a.public_key(), b.public_key(), c.public_key()],
        100,
    )]);

    let inner = cf_kinds_authors(&[3], &[&a.public_key().to_hex()]);
    let atom_a = cf_kinds_authors(&[1], &[&a.public_key().to_hex()]);
    let atom_b = cf_kinds_authors(&[1], &[&b.public_key().to_hex()]);
    let atom_c = cf_kinds_authors(&[1], &[&c.public_key().to_hex()]);
    let demand = h.demand();
    assert_eq!(
        demand,
        BTreeSet::from([
            inner.clone(),
            atom_a.clone(),
            atom_b.clone(),
            atom_c.clone()
        ])
    );

    let before = h.metrics();
    let delta = h.deliver(vec![kind3(
        &a,
        &[a.public_key(), b.public_key(), d.public_key()],
        101,
    )]);
    let atom_d = cf_kinds_authors(&[1], &[&d.public_key().to_hex()]);

    assert_eq!(
        delta.ops,
        vec![
            DemandOp::Close(outbox_atom(atom_c.clone())),
            DemandOp::Open(outbox_atom(atom_d.clone()))
        ]
    );
    let after = h.metrics();
    assert_eq!(after.atoms_closed - before.atoms_closed, 1);
    assert_eq!(after.atoms_opened - before.atoms_opened, 1);
    assert_eq!(after.recompute_passes - before.recompute_passes, 1);

    let demand = h.demand();
    assert!(demand.contains(&inner), "inner kind:3 atom untouched");
    assert!(demand.contains(&atom_a), "A untouched");
    assert!(demand.contains(&atom_b), "B untouched");
    assert!(!demand.contains(&atom_c), "C closed");
    assert!(demand.contains(&atom_d), "D opened");
}

// ---- 2. depth2_nip29_groups_cascade_one_level ---------------------------

#[test]
fn depth2_nip29_groups_cascade_one_level() {
    let mut h = Harness::new();
    let a = Keys::generate();

    h.set_active(Some(a.public_key()));
    let (_handle, _open_delta) = h.subscribe(LiveQuery::from_filter(nip29_groups_filter()));
    h.deliver(vec![
        kind39002(&a, "g1", &[a.public_key()], 100),
        kind39002(&a, "g2", &[a.public_key()], 100),
    ]);

    let inner = cf_kinds_tag(&[39_002], 'p', &[&a.public_key().to_hex()]);
    let outer_kinds = [39_000u16, 39_001, 39_002];
    let outer_g1 = cf_kinds_tag(&outer_kinds, 'd', &["g1"]);
    let outer_g2 = cf_kinds_tag(&outer_kinds, 'd', &["g2"]);
    let demand = h.demand();
    assert!(demand.contains(&inner));
    assert!(demand.contains(&outer_g1));
    assert!(demand.contains(&outer_g2));

    let snapshot_before = h.graph_snapshot().nodes.len();
    let before = h.metrics();
    let delta = h.deliver(vec![kind39002(&a, "g3", &[a.public_key()], 101)]);
    let outer_g3 = cf_kinds_tag(&outer_kinds, 'd', &["g3"]);

    assert_eq!(
        delta.ops,
        vec![DemandOp::Open(public_atom(outer_g3.clone()))]
    );
    let after = h.metrics();
    assert_eq!(
        after.atoms_opened - before.atoms_opened,
        1,
        "zero churn: only g3 opens"
    );
    assert_eq!(
        after.atoms_closed - before.atoms_closed,
        0,
        "inner atom unchanged, zero churn"
    );
    assert_eq!(
        after.nodes_recomputed - before.nodes_recomputed,
        2,
        "cascade depth == 1: exactly the Derived node + the outer FilterNode"
    );

    // recompile-not-reopen: the graph's node structure itself is untouched.
    assert_eq!(h.graph_snapshot().nodes.len(), snapshot_before);
    assert!(
        h.demand().contains(&inner),
        "inner atom still present, zero churn"
    );
}

// ---- 3. identity_reroot_closes_old_before_new ---------------------------

#[test]
fn identity_reroot_closes_old_before_new() {
    let mut h = Harness::new();
    let a = Keys::generate();
    let b = Keys::generate();
    let e = Keys::generate();
    let f = Keys::generate();

    h.set_active(Some(a.public_key()));
    let (_handle, _open_delta) = h.subscribe(LiveQuery::from_filter(my_follows_filter()));
    h.deliver(vec![kind3(&a, &[a.public_key(), b.public_key()], 100)]);

    let old_inner = cf_kinds_authors(&[3], &[&a.public_key().to_hex()]);
    let old_a = cf_kinds_authors(&[1], &[&a.public_key().to_hex()]);
    let old_b = cf_kinds_authors(&[1], &[&b.public_key().to_hex()]);
    let demand_before = h.demand();
    assert!(demand_before.contains(&old_inner));
    assert!(demand_before.contains(&old_a));
    assert!(demand_before.contains(&old_b));

    let delta = h.set_active(Some(b.public_key()));

    // Every Close index precedes every Open index.
    let mut seen_open = false;
    for op in &delta.ops {
        match op {
            DemandOp::Open(_) => seen_open = true,
            DemandOp::Close(_) => assert!(!seen_open, "a Close appeared after an Open"),
        }
    }

    let closes: BTreeSet<ContextualAtom> = delta.closed().into_iter().cloned().collect();
    assert_eq!(
        closes,
        BTreeSet::from([
            outbox_atom(old_inner.clone()),
            outbox_atom(old_a.clone()),
            outbox_atom(old_b.clone())
        ]),
        "all old atoms closed"
    );
    // Reverse-of-open order: the inner (foundation, opened first at
    // construction) is closed LAST.
    assert_eq!(
        delta.closed().last(),
        Some(&&outbox_atom(old_inner.clone()))
    );

    let new_inner = cf_kinds_authors(&[3], &[&b.public_key().to_hex()]);
    assert_eq!(
        delta.opened(),
        vec![&outbox_atom(new_inner.clone())],
        "only the new inner atom opens"
    );

    let demand_after = h.demand();
    let a_hex = a.public_key().to_hex();
    assert!(
        !demand_after
            .iter()
            .any(|cf| cf.authors.as_ref().is_some_and(|s| s.contains(&a_hex))),
        "no atom mentioning the old pubkey survives -- no cross-account leak"
    );
    assert!(demand_after.contains(&new_inner));

    let delta2 = h.deliver(vec![kind3(&b, &[e.public_key(), f.public_key()], 100)]);
    let atom_e = cf_kinds_authors(&[1], &[&e.public_key().to_hex()]);
    let atom_f = cf_kinds_authors(&[1], &[&f.public_key().to_hex()]);
    let opened: BTreeSet<ContextualAtom> = delta2.opened().into_iter().cloned().collect();
    assert_eq!(
        opened,
        BTreeSet::from([outbox_atom(atom_e), outbox_atom(atom_f)])
    );
}

// ---- 4. stale_older_kind3_rejected_without_firing -----------------------

#[test]
fn stale_older_kind3_rejected_without_firing() {
    let mut h = Harness::new();
    let a = Keys::generate();

    h.set_active(Some(a.public_key()));
    let (_handle, _open_delta) = h.subscribe(LiveQuery::from_filter(my_follows_filter()));
    h.deliver(vec![kind3(
        &a,
        &[
            a.public_key(),
            Keys::generate().public_key(),
            Keys::generate().public_key(),
        ],
        100,
    )]);

    let before = h.metrics();
    let demand_before = h.demand();
    let delta = h.deliver(vec![kind3(
        &a,
        &[Keys::generate().public_key(), Keys::generate().public_key()],
        50, // older than the winner already in the store
    )]);

    assert!(delta.is_empty());
    let after = h.metrics();
    assert_eq!(after.recompute_passes, before.recompute_passes);
    assert_eq!(h.demand(), demand_before);
}

// ---- 5. duplicate_delivery_no_fire --------------------------------------

#[test]
fn duplicate_delivery_no_fire() {
    let mut h = Harness::new();
    let a = Keys::generate();

    h.set_active(Some(a.public_key()));
    let (_handle, _open_delta) = h.subscribe(LiveQuery::from_filter(my_follows_filter()));
    let ev = kind3(&a, &[a.public_key()], 100);
    h.deliver(vec![ev.clone()]);

    let before = h.metrics();
    let delta = h.deliver(vec![ev]);

    assert!(delta.is_empty());
    assert_eq!(h.metrics().recompute_passes, before.recompute_passes);
}

// ---- 6. unchanged_set_ingest_empty_delta --------------------------------

#[test]
fn unchanged_set_ingest_empty_delta() {
    let mut h = Harness::new();
    let a = Keys::generate();
    let b = Keys::generate();
    let c = Keys::generate();

    h.set_active(Some(a.public_key()));
    let (_handle, _open_delta) = h.subscribe(LiveQuery::from_filter(my_follows_filter()));
    h.deliver(vec![kind3(
        &a,
        &[a.public_key(), b.public_key(), c.public_key()],
        100,
    )]);

    let before = h.metrics();
    // Same members, newer timestamp: supersedes, but the projected set is
    // unchanged -- set-diff, not event-diff, gates downstream.
    let delta = h.deliver(vec![kind3(
        &a,
        &[a.public_key(), b.public_key(), c.public_key()],
        101,
    )]);

    assert!(delta.is_empty());
    let after = h.metrics();
    assert_eq!(after.atoms_opened, before.atoms_opened);
    assert_eq!(after.atoms_closed, before.atoms_closed);
}

// ---- 7. concurrent_depth2_changes_batch_one_delta -----------------------

#[test]
fn concurrent_depth2_changes_batch_one_delta() {
    let mut h = Harness::new();
    let a = Keys::generate();

    h.set_active(Some(a.public_key()));
    let (_handle, _open_delta) = h.subscribe(LiveQuery::from_filter(nip29_groups_filter()));
    h.deliver(vec![kind39002(&a, "g1", &[a.public_key()], 100)]);

    let before = h.metrics();
    // One batch: add g3, and remove-by-supersede g1 (a newer g1 event that
    // no longer lists A as a member, so it no longer matches #p:A_pk).
    let delta = h.deliver(vec![
        kind39002(&a, "g3", &[a.public_key()], 100),
        kind39002(&a, "g1", &[], 101),
    ]);

    let outer_kinds = [39_000u16, 39_001, 39_002];
    let g1 = cf_kinds_tag(&outer_kinds, 'd', &["g1"]);
    let g3 = cf_kinds_tag(&outer_kinds, 'd', &["g3"]);
    assert_eq!(
        delta.ops,
        vec![
            DemandOp::Close(public_atom(g1)),
            DemandOp::Open(public_atom(g3))
        ]
    );

    let after = h.metrics();
    assert_eq!(
        after.recompute_passes - before.recompute_passes,
        1,
        "one compile-invalidation for the whole batch"
    );
}

// ---- 8. identical_descriptors_share_graph -------------------------------

#[test]
fn identical_descriptors_share_graph() {
    let mut h = Harness::new();
    let a = Keys::generate();

    h.set_active(Some(a.public_key()));
    let (handle1, _delta1) = h.subscribe(LiveQuery::from_filter(my_follows_filter()));
    let (handle2, delta2) = h.subscribe(LiveQuery::from_filter(my_follows_filter()));
    assert!(
        delta2.is_empty(),
        "second subscribe to an identical descriptor shares the graph"
    );

    let demand_with_both = h.demand();
    let inner = cf_kinds_authors(&[3], &[&a.public_key().to_hex()]);
    assert_eq!(demand_with_both, BTreeSet::from([inner.clone()]));

    let close_first = h.unsubscribe(handle1.id());
    assert!(
        close_first.is_empty(),
        "refcount 1 remains: nothing actually closes yet"
    );
    assert_eq!(h.demand(), demand_with_both);

    let close_second = h.unsubscribe(handle2.id());
    assert_eq!(close_second.closed(), vec![&outbox_atom(inner.clone())]);
    assert!(h.demand().is_empty());
}

// ---- 9. follows_minus_mutes_surgical ------------------------------------

#[test]
fn follows_minus_mutes_surgical() {
    let mut h = Harness::new();
    let a = Keys::generate();
    let b = Keys::generate();
    let c = Keys::generate();

    h.set_active(Some(a.public_key()));
    let (_handle, _open_delta) = h.subscribe(LiveQuery::from_filter(follows_minus_mutes_filter()));
    h.deliver(vec![kind3(
        &a,
        &[a.public_key(), b.public_key(), c.public_key()],
        100,
    )]);

    let atom_a = cf_kinds_authors(&[1], &[&a.public_key().to_hex()]);
    let atom_b = cf_kinds_authors(&[1], &[&b.public_key().to_hex()]);
    let atom_c = cf_kinds_authors(&[1], &[&c.public_key().to_hex()]);
    assert_eq!(
        h.demand()
            .into_iter()
            .filter(|cf| cf.kinds.as_ref() == Some(&BTreeSet::from([1u16])))
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([atom_a.clone(), atom_b.clone(), atom_c.clone()])
    );

    let before = h.metrics();
    let delta = h.deliver(vec![kind10000_mutes(&a, &[a.public_key()], 100)]);

    assert_eq!(
        delta.ops,
        vec![DemandOp::Close(outbox_atom(atom_a.clone()))]
    );
    let after = h.metrics();
    assert_eq!(after.atoms_opened - before.atoms_opened, 0);
    assert_eq!(after.atoms_closed - before.atoms_closed, 1);
    assert_eq!(after.recompute_passes - before.recompute_passes, 1);

    let demand = h.demand();
    assert!(!demand.contains(&atom_a));
    assert!(demand.contains(&atom_b), "B untouched");
    assert!(demand.contains(&atom_c), "C untouched");
}

// ---- 11. address_coord_fans_out_per_coordinate --------------------------

#[test]
fn address_coord_fans_out_per_coordinate() {
    let mut h = Harness::new();
    let a = Keys::generate();

    h.set_active(Some(a.public_key()));
    let (_handle, _open_delta) = h.subscribe(LiveQuery::from_filter(address_coord_filter()));
    h.deliver(vec![
        addressable(&a, 30_003, "g1", 100),
        addressable(&a, 30_003, "g2", 100),
    ]);

    let a_hex = a.public_key().to_hex();
    let atom_g1 = cf_coord(30_003, &a_hex, "g1");
    let atom_g2 = cf_coord(30_003, &a_hex, "g2");
    let demand = h.demand();
    assert!(demand.contains(&atom_g1));
    assert!(demand.contains(&atom_g2));

    // Co-pinned: kinds/authors/#d travel together in ONE atom per
    // coordinate, never as an independent cartesian of separate
    // field-sets.
    // Only the co-pinned (AddressCoord-produced) atoms carry a `d` tag at
    // all -- the inner FilterNode's own atom (kinds:30003, authors:A_pk,
    // no `#d`) is a different, unrelated demand atom and must be excluded
    // from this check.
    let d_tag = IndexedTagName::new('d').unwrap();
    for atom in &demand {
        if atom.tags.contains_key(&d_tag) {
            assert!(atom.kinds.as_ref().is_some_and(|k| k.contains(&30_003)));
            assert_eq!(atom.authors.as_ref().unwrap().len(), 1);
            assert_eq!(atom.tags.get(&d_tag).unwrap().len(), 1);
        }
    }

    let before = h.metrics();
    let delta = h.deliver(vec![addressable(&a, 30_003, "g3", 100)]);
    let atom_g3 = cf_coord(30_003, &a_hex, "g3");

    assert_eq!(delta.ops, vec![DemandOp::Open(outbox_atom(atom_g3))]);
    let after = h.metrics();
    assert_eq!(after.atoms_opened - before.atoms_opened, 1);
    assert_eq!(after.atoms_closed - before.atoms_closed, 0);
}

// ---- 12. arbitrary_depth1_shape_needs_no_engine_change ------------------

#[test]
fn arbitrary_depth1_shape_needs_no_engine_change() {
    let mut h = Harness::new();
    let a = Keys::generate();
    let e1 = dummy_event_id("bookmark-1");
    let e2 = dummy_event_id("bookmark-2");

    h.set_active(Some(a.public_key()));
    let (_handle, _open_delta) = h.subscribe(LiveQuery::from_filter(bookmarks_filter()));
    let delta = h.deliver(vec![kind10003_bookmarks(&a, &[e1, e2], 100)]);

    let atom_e1 = cf_kinds_tag(&[1], 'e', &[&e1.to_hex()]);
    let atom_e2 = cf_kinds_tag(&[1], 'e', &[&e2.to_hex()]);
    let opened: BTreeSet<ContextualAtom> = delta.opened().into_iter().cloned().collect();
    assert_eq!(
        opened,
        BTreeSet::from([public_atom(atom_e1.clone()), public_atom(atom_e2.clone())])
    );
    assert!(h.demand().contains(&atom_e1));
    assert!(h.demand().contains(&atom_e2));
}

// ---- 13/14. Retraction seam (#34,
// retraction-and-negative-deltas.md §1.2/§1.4) ---------------------------

/// The smoking-gun case (issue #34 / design §0 finding 1): a kind:5
/// deletion's OWN kind (5) matches no inner filter at all -- the removed
/// member (a kind:39002 group-membership event) does, but under the OLD
/// add-only dirty-seed loop nothing about the ARRIVING deletion event would
/// ever have planted a seed for it. Before #34 this ghosted `g1` in the
/// outer derived set forever (`recompute_node` re-queries the store, which
/// no longer holds g1, but the seed to trigger that recompute never fired);
/// after #34, `removed` feeds the SAME `match_event` test `inserted`
/// always got, so the retraction is caught with zero shape-luck involved.
#[test]
fn derived_set_retracts_deleted_member_that_new_winner_does_not_match() {
    let mut h = Harness::new();
    let a = Keys::generate();

    h.set_active(Some(a.public_key()));
    let (_handle, _open_delta) = h.subscribe(LiveQuery::from_filter(nip29_groups_filter()));
    let g1_event = kind39002(&a, "g1", &[a.public_key()], 100);
    let g1_id = g1_event.id;
    h.deliver(vec![g1_event, kind39002(&a, "g2", &[a.public_key()], 100)]);

    let outer_kinds = [39_000u16, 39_001, 39_002];
    let outer_g1 = cf_kinds_tag(&outer_kinds, 'd', &["g1"]);
    let outer_g2 = cf_kinds_tag(&outer_kinds, 'd', &["g2"]);
    assert!(h.demand().contains(&outer_g1), "g1 open before delete");
    assert!(h.demand().contains(&outer_g2), "g2 unaffected, sanity");

    let delta = h.deliver(vec![deletion(&a, &[g1_id], 200)]);

    assert!(
        delta
            .ops
            .contains(&DemandOp::Close(public_atom(outer_g1.clone()))),
        "deleting g1's membership event must close its derived atom even \
         though the deleting kind:5 event itself matches no inner filter: \
         {:?}",
        delta.ops
    );
    assert!(
        !h.demand().contains(&outer_g1),
        "g1 must be gone from active demand after the delete"
    );
    assert!(h.demand().contains(&outer_g2), "g2 must survive untouched");
}

/// Replace-not-rebuild extends to retraction (design §1.2's witness): only
/// the retracted member's own atom churns -- the inner (kind:39002, #p)
/// atom's SHAPE never changes (g2/g3 still match it), so it must show zero
/// open/close activity, and exactly one atom closes for the one deleted
/// member.
#[test]
fn metrics_witness_only_retracted_member_atoms_churn() {
    let mut h = Harness::new();
    let a = Keys::generate();

    h.set_active(Some(a.public_key()));
    let (_handle, _open_delta) = h.subscribe(LiveQuery::from_filter(nip29_groups_filter()));
    let g1_event = kind39002(&a, "g1", &[a.public_key()], 100);
    let g1_id = g1_event.id;
    h.deliver(vec![
        g1_event,
        kind39002(&a, "g2", &[a.public_key()], 100),
        kind39002(&a, "g3", &[a.public_key()], 100),
    ]);

    let inner = cf_kinds_tag(&[39_002], 'p', &[&a.public_key().to_hex()]);
    assert!(h.demand().contains(&inner));

    let before = h.metrics();
    let delta = h.deliver(vec![deletion(&a, &[g1_id], 200)]);
    let outer_kinds = [39_000u16, 39_001, 39_002];
    let outer_g1 = cf_kinds_tag(&outer_kinds, 'd', &["g1"]);

    assert_eq!(delta.ops, vec![DemandOp::Close(public_atom(outer_g1))]);
    let after = h.metrics();
    assert_eq!(
        after.atoms_closed - before.atoms_closed,
        1,
        "replace-not-rebuild: only g1's own atom closes"
    );
    assert_eq!(
        after.atoms_opened - before.atoms_opened,
        0,
        "no atom opens on a pure retraction"
    );
    assert!(
        h.demand().contains(&inner),
        "the inner (kind:39002, #p) atom is untouched -- same shape, still \
         matched by g2/g3"
    );
}
