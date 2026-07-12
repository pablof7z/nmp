//! Unit U2 (`docs/design/crashsafe-accepted-2-3-plan.md` §1.2 / §6-U2,
//! issues #2/#3 under epic #23): the resolver's LOCAL-authorship add path,
//! [`Engine::accept_local`]. A locally-composed write enters the ONE store
//! through the `EventStore::accept_write` door and its [`AcceptOutcome`] is
//! sorted into the SAME `react` machinery a relay insert's `InsertOutcome`
//! feeds — so an optimistic write is query-visible immediately, with **no
//! app optimistic mirror** and no second visibility path.
//!
//! These tests drive the REAL door through [`Harness::accept`] (mirroring
//! `contract.rs`'s `deliver`-driven tests) over the `my_follows` `Derived`
//! filter, so every case also proves Derived-over-kind:3 re-resolution off a
//! purely local edit. They assert both the store `AcceptOutcome`
//! classification and the emitted `DemandDelta` + `Metrics` witness
//! (`atoms_opened + atoms_closed == |symmetric diff|`).

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::{
    Binding, ConcreteFilter, Demand, DemandOp, Derived, Filter, IdentityField, IndexedTagName,
    Selector,
};
use nmp_resolver::testkit::{accept_write_of, deletion, kind3, Harness};
use nmp_resolver::LiveQuery;
use nmp_store::AcceptOutcome;
use nostr::Keys;

// ---- local helpers (each integration-test file is its own crate; these
// mirror contract.rs's test-local builders) ------------------------------

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

/// `kinds:[1], authors := Derived(inner=(kinds:[3], authors:[Reactive]),
/// project=Tag(p))` — "my follows" (identical to contract.rs's fixture).
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

/// `kinds:[1], #e := Derived(inner=(kinds:[5], authors:[Reactive]),
/// project=Tag(e))` — "notes e-tagging the things I've deleted". The inner
/// resolves to the active pubkey's OWN kind:5 deletions; projecting their
/// `e` tags binds each deleted event id into an `#e` atom. Used to make the
/// kind:5 deletion row's entry into `inserted` observable at the demand
/// level (when a local kind:5 lands, this Derived opens an atom for its
/// e-tagged target).
fn deletions_by_etag_filter() -> Filter {
    let mut tags = BTreeMap::new();
    tags.insert(
        IndexedTagName::new('e').unwrap(),
        Binding::Derived(Box::new(Derived {
            inner: Demand::from_filter(Filter {
                kinds: Some(BTreeSet::from([5u16])),
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

/// The `DemandOp::Open` atoms of a delta, as a SELECTION-only set (op ORDER
/// is the resolver's internal `BTreeSet` ordering — these tests assert on
/// the SET, keeping them robust to that ordering while still exact on
/// membership). `DemandOp` carries a full `ContextualAtom` (#106), but
/// every fixture in this file is `my_follows_filter()`-shaped (uniformly
/// `AuthorOutboxes`), so these tests are about atom identity, not context;
/// extracting `.filter` keeps every existing assertion unchanged.
fn opened(delta: &nmp_grammar::DemandDelta) -> BTreeSet<ConcreteFilter> {
    delta
        .ops
        .iter()
        .filter_map(|op| match op {
            DemandOp::Open(atom) => Some(atom.filter.clone()),
            DemandOp::Close(_) => None,
        })
        .collect()
}

fn closed(delta: &nmp_grammar::DemandDelta) -> BTreeSet<ConcreteFilter> {
    delta
        .ops
        .iter()
        .filter_map(|op| match op {
            DemandOp::Close(atom) => Some(atom.filter.clone()),
            DemandOp::Open(_) => None,
        })
        .collect()
}

// ---- 1. accept_local seeds the Derived add path (Inserted) --------------

/// An optimistic LOCAL kind:3 edit — never seen by any relay — drives the
/// `Derived` "my follows" re-resolution exactly as a relay-observed kind:3
/// would: the three followed authors' outer atoms open in ONE recompute
/// pass, so the composition is query-visible with zero relay round-trip
/// (#2's "no app optimistic mirror"). Metrics witness: opened + closed ==
/// |symmetric diff| == 3.
#[test]
fn accept_local_seeds_the_derived_add_path() {
    let mut h = Harness::new();
    let a = Keys::generate();
    let b = Keys::generate();
    let c = Keys::generate();

    h.set_active(Some(a.public_key()));
    let (_handle, _open_delta) = h.subscribe(LiveQuery::from_filter(my_follows_filter()));

    let inner = cf_kinds_authors(&[3], &[&a.public_key().to_hex()]);
    let atom_a = cf_kinds_authors(&[1], &[&a.public_key().to_hex()]);
    let atom_b = cf_kinds_authors(&[1], &[&b.public_key().to_hex()]);
    let atom_c = cf_kinds_authors(&[1], &[&c.public_key().to_hex()]);

    let before = h.metrics();
    let (outcome, delta) = h.accept(accept_write_of(
        kind3(&a, &[a.public_key(), b.public_key(), c.public_key()], 100),
        100,
    ));

    assert!(
        matches!(outcome, AcceptOutcome::Inserted { .. }),
        "a fresh local kind:3 with no address competition is Inserted -- got {outcome:?}"
    );
    assert_eq!(
        opened(&delta),
        BTreeSet::from([atom_a.clone(), atom_b.clone(), atom_c.clone()]),
        "the three followed authors' outer atoms open off the LOCAL edit"
    );
    assert!(
        closed(&delta).is_empty(),
        "nothing closes on a first insert"
    );

    let after = h.metrics();
    assert_eq!(after.atoms_opened - before.atoms_opened, 3);
    assert_eq!(after.atoms_closed - before.atoms_closed, 0);
    assert_eq!(
        after.recompute_passes - before.recompute_passes,
        1,
        "one accept => one react pass"
    );
    // Metrics witness: atoms_opened + atoms_closed == |symmetric diff|.
    // The symmetric diff is {a,b,c} newly matching, nothing removed => 3.
    assert_eq!(
        (after.atoms_opened - before.atoms_opened) + (after.atoms_closed - before.atoms_closed),
        3,
        "witness: opened + closed == |symmetric diff|"
    );

    let demand = h.demand();
    assert!(demand.contains(&inner), "inner kind:3 atom present");
    for atom in [&atom_a, &atom_b, &atom_c] {
        assert!(
            demand.contains(atom),
            "optimistic local write is query-visible with no relay echo: {atom:?}"
        );
    }
}

// ---- 2. superseding local edit adds + removes through ONE react ----------

/// A NEWER local kind:3 (same author, later `created_at`) supersedes the
/// pending predecessor at the same replaceable address: `accept_write`
/// returns `Superseded { replaced }`, `accept_local` feeds BOTH the new row
/// (`inserted`) and the evicted predecessor (`removed`) into ONE `react`, so
/// the dropped follow's atom closes and the added follow's atom opens in a
/// single pass — the §1 negative-delta lane running for a local edit. This
/// is the load-bearing U2 proof: the remove rides the SAME recompute as the
/// add, never a second call. Witness: opened + closed == |symmetric diff|
/// == 2.
#[test]
fn superseding_local_edit_adds_and_removes_through_one_react() {
    let mut h = Harness::new();
    let a = Keys::generate();
    let b = Keys::generate();
    let c = Keys::generate();
    let d = Keys::generate();

    h.set_active(Some(a.public_key()));
    let (_handle, _open_delta) = h.subscribe(LiveQuery::from_filter(my_follows_filter()));

    // First optimistic edit: follows = {a, b, c}.
    h.accept(accept_write_of(
        kind3(&a, &[a.public_key(), b.public_key(), c.public_key()], 100),
        100,
    ));

    let atom_c = cf_kinds_authors(&[1], &[&c.public_key().to_hex()]);
    let atom_d = cf_kinds_authors(&[1], &[&d.public_key().to_hex()]);
    let atom_a = cf_kinds_authors(&[1], &[&a.public_key().to_hex()]);
    let atom_b = cf_kinds_authors(&[1], &[&b.public_key().to_hex()]);

    // Superseding edit: follows = {a, b, d} — drops c, adds d.
    let before = h.metrics();
    let (outcome, delta) = h.accept(accept_write_of(
        kind3(&a, &[a.public_key(), b.public_key(), d.public_key()], 101),
        101,
    ));

    assert!(
        matches!(outcome, AcceptOutcome::Superseded { .. }),
        "a newer local kind:3 evicts the pending predecessor -- got {outcome:?}"
    );
    assert_eq!(
        closed(&delta),
        BTreeSet::from([atom_c.clone()]),
        "the dropped follow c closes -- the negative delta off the evicted predecessor"
    );
    assert_eq!(
        opened(&delta),
        BTreeSet::from([atom_d.clone()]),
        "the added follow d opens"
    );

    let after = h.metrics();
    assert_eq!(after.atoms_closed - before.atoms_closed, 1);
    assert_eq!(after.atoms_opened - before.atoms_opened, 1);
    assert_eq!(
        after.recompute_passes - before.recompute_passes,
        1,
        "add AND remove ride ONE react pass, not two"
    );
    // Metrics witness: atoms_opened + atoms_closed == |symmetric diff|.
    // Symmetric diff is {c removed, d added} => 2.
    assert_eq!(
        (after.atoms_opened - before.atoms_opened) + (after.atoms_closed - before.atoms_closed),
        2,
        "witness: opened + closed == |symmetric diff|"
    );

    let demand = h.demand();
    assert!(
        demand.contains(&atom_a) && demand.contains(&atom_b),
        "a,b untouched"
    );
    assert!(demand.contains(&atom_d), "d present");
    assert!(!demand.contains(&atom_c), "c gone");
}

// ---- 3. an older local edit loses its address race (Stale) --------------

/// A local kind:3 whose `created_at` is OLDER than the pending winner at the
/// same address loses the race: `accept_write` returns `Stale` (journaled —
/// still gets its own receipt/signing — but produces no pending row), and
/// `accept_local` yields an EMPTY delta with no graph churn. Proves the
/// `Stale => {}` arm: a lost race never phantom-opens an atom.
#[test]
fn older_local_edit_is_stale_and_yields_empty_delta() {
    let mut h = Harness::new();
    let a = Keys::generate();
    let b = Keys::generate();
    let c = Keys::generate();

    h.set_active(Some(a.public_key()));
    let (_handle, _open_delta) = h.subscribe(LiveQuery::from_filter(my_follows_filter()));

    // Winner at t=101: follows = {a, b}.
    h.accept(accept_write_of(
        kind3(&a, &[a.public_key(), b.public_key()], 101),
        101,
    ));

    let atom_c = cf_kinds_authors(&[1], &[&c.public_key().to_hex()]);
    let before = h.metrics();
    // Older edit at t=100: follows = {a, c} — must lose to the t=101 winner.
    let (outcome, delta) = h.accept(accept_write_of(
        kind3(&a, &[a.public_key(), c.public_key()], 100),
        102,
    ));

    assert!(
        matches!(outcome, AcceptOutcome::Stale { .. }),
        "an older local kind:3 loses the address race -- got {outcome:?}"
    );
    assert!(
        delta.ops.is_empty(),
        "a Stale accept produces no demand delta -- got {:?}",
        delta.ops
    );
    let after = h.metrics();
    assert_eq!(
        after.recompute_passes - before.recompute_passes,
        0,
        "a Stale accept triggers no recompute pass"
    );
    assert!(
        !h.demand().contains(&atom_c),
        "the losing edit's follow c never enters demand"
    );
}

// ---- 4. re-accepting the identical body is a Duplicate (empty delta) -----

/// Accepting the EXACT same frozen body twice (same id — NIP-01's id never
/// depends on `sig`) is a `Duplicate`: the row is already reflected in the
/// store (a distinct intent still joins its owner set and gets a fresh
/// receipt, but nothing new becomes query-visible), so `accept_local` yields
/// an empty delta. Proves the `Duplicate => {}` arm.
#[test]
fn duplicate_local_accept_yields_empty_delta() {
    let mut h = Harness::new();
    let a = Keys::generate();
    let b = Keys::generate();

    h.set_active(Some(a.public_key()));
    let (_handle, _open_delta) = h.subscribe(LiveQuery::from_filter(my_follows_filter()));

    let follow_list = kind3(&a, &[a.public_key(), b.public_key()], 100);
    h.accept(accept_write_of(follow_list.clone(), 100));

    let before = h.metrics();
    // Identical body => identical id => Duplicate.
    let (outcome, delta) = h.accept(accept_write_of(follow_list, 101));

    assert!(
        matches!(outcome, AcceptOutcome::Duplicate { .. }),
        "the exact same event id is a Duplicate -- got {outcome:?}"
    );
    assert!(
        delta.ops.is_empty(),
        "a Duplicate accept produces no demand delta -- got {:?}",
        delta.ops
    );
    let after = h.metrics();
    assert_eq!(
        after.recompute_passes - before.recompute_passes,
        0,
        "a Duplicate accept triggers no recompute pass"
    );
}

// ---- 5. a local kind:5 is Kind5Processed: row -> inserted, hidden -------
//         targets -> removed, in ONE react ---------------------------------

/// The load-bearing `Kind5Processed` arm. TWO live queries observe a single
/// local deletion accept from both sides at once:
///
/// - `my_follows` (kind:3 `Derived`) currently resolves to {b, c} from a
///   pending local kind:3. When the kind:5 hides that kind:3, the hidden
///   target enters `react`'s `removed`, the inner re-resolves to empty, and
///   the follow atoms b/c CLOSE.
/// - `deletions_by_etag` (kind:5 `Derived`) sees the pending kind:5 row enter
///   `react`'s `inserted` and OPENS an `#e` atom for the deleted id.
///
/// Both happen in ONE `react` (one recompute pass), proving `accept_local`
/// feeds the `Kind5Processed { row, hidden }` outcome's row into `inserted`
/// and its hidden targets into `removed` symmetrically — the §1 negative-
/// delta lane for a local deletion, with the `Metrics` witness holding.
#[test]
fn local_kind5_processed_inserts_row_and_removes_hidden_targets_in_one_react() {
    let mut h = Harness::new();
    let a = Keys::generate();
    let b = Keys::generate();
    let c = Keys::generate();

    h.set_active(Some(a.public_key()));
    let (_h_follows, _d1) = h.subscribe(LiveQuery::from_filter(my_follows_filter()));
    let (_h_dels, _d2) = h.subscribe(LiveQuery::from_filter(deletions_by_etag_filter()));

    // A pending local kind:3 => my_follows resolves to {b, c}.
    let follow_list = kind3(&a, &[b.public_key(), c.public_key()], 100);
    let follow_list_id = follow_list.id;
    let (outcome1, _d) = h.accept(accept_write_of(follow_list, 100));
    assert!(matches!(outcome1, AcceptOutcome::Inserted { .. }));

    let atom_b = cf_kinds_authors(&[1], &[&b.public_key().to_hex()]);
    let atom_c = cf_kinds_authors(&[1], &[&c.public_key().to_hex()]);
    let deleted_atom = cf_kinds_tag(&[1], 'e', &[&follow_list_id.to_hex()]);
    assert!(h.demand().contains(&atom_b) && h.demand().contains(&atom_c));
    assert!(
        !h.demand().contains(&deleted_atom),
        "no deletion yet: the e-tag atom is not open"
    );

    // A local kind:5 deleting that kind:3 => Kind5Processed.
    let before = h.metrics();
    let (outcome2, delta) = h.accept(accept_write_of(deletion(&a, &[follow_list_id], 101), 101));

    assert!(
        matches!(outcome2, AcceptOutcome::Kind5Processed { .. }),
        "a local kind:5 deletion is Kind5Processed -- got {outcome2:?}"
    );
    // Hidden target (kind:3) entered `removed`: the follow atoms close.
    assert_eq!(
        closed(&delta),
        BTreeSet::from([atom_b.clone(), atom_c.clone()]),
        "the hidden kind:3's suppression closes the follow atoms b,c (removed side)"
    );
    // Deletion row (kind:5) entered `inserted`: the e-tag atom opens.
    assert_eq!(
        opened(&delta),
        BTreeSet::from([deleted_atom.clone()]),
        "the pending kind:5 row opens the deletions-by-e-tag atom (inserted side)"
    );

    let after = h.metrics();
    assert_eq!(
        after.recompute_passes - before.recompute_passes,
        1,
        "insertion AND suppression-removal ride ONE react pass, not two"
    );
    // Witness: opened + closed == |symmetric diff| == {deleted_id opened} + {b,c closed} == 3.
    assert_eq!(
        (after.atoms_opened - before.atoms_opened) + (after.atoms_closed - before.atoms_closed),
        3,
        "witness: opened + closed == |symmetric diff|"
    );

    let demand = h.demand();
    assert!(
        !demand.contains(&atom_b) && !demand.contains(&atom_c),
        "the deleted follow list's members are gone from demand"
    );
    assert!(
        demand.contains(&deleted_atom),
        "the deletion is query-visible: its e-tag atom is open"
    );
}
