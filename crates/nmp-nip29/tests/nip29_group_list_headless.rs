//! #108: `current_account_group_list_demand()` proven against a REAL
//! `EngineCore` -- signed-out state yields no current-account kind-10009
//! demand, and signing in (or rerooting to a different account)
//! reconstructs it correctly. Moved here from `nmp` by #1707 alongside the
//! demand function itself: `nmp` should not carry a NIP-29-specific proof
//! any more than NIP-29-specific code. Drives `nmp-engine`'s own reducer
//! directly -- an ordinary dev-dependency edge, the same in-workspace
//! access `nmp`'s own headless reducer tests use.

use nmp_engine::core::{EngineCore, EngineMsg};
use nmp_grammar::ContextualAtom;
use nmp_grammar::LiveQuery;
use nmp_router_testkit::FixtureRoutingFacts;
use nmp_store::RedbStore;
use nostr::{Keys, RelayUrl};

fn new_core(dir: FixtureRoutingFacts) -> EngineCore {
    EngineCore::new_with_fixture_routing_facts(
        RedbStore::temporary().expect("temporary Redb store"),
        dir,
        10,
    )
}

fn kind_10009_atoms(atoms: &std::collections::BTreeSet<ContextualAtom>) -> usize {
    atoms
        .iter()
        .filter(|a| a.filter.kinds.as_ref().is_some_and(|k| k.contains(&10009)))
        .count()
}

#[test]
fn signed_out_current_account_group_list_demand_resolves_to_zero_atoms() {
    let mut core = new_core(FixtureRoutingFacts::new());
    let _ = core.handle(EngineMsg::Subscribe(LiveQuery::single(
        nmp_nip29::current_account_group_list_demand(),
    )));
    assert_eq!(
        kind_10009_atoms(&core.active_demand()),
        0,
        "signed-out (no active pubkey) must yield zero kind:10009 atoms, never a \
         fabricated/empty-author subscription"
    );
}

#[test]
fn signing_in_reconstructs_the_current_account_kind_10009_demand() {
    let a = Keys::generate();
    let relay = RelayUrl::parse("wss://relay-a.example.com").unwrap();
    let dir = FixtureRoutingFacts::new().with_outbound_routes(a.public_key(), [relay]);
    let mut core = new_core(dir);

    let _ = core.handle(EngineMsg::Subscribe(LiveQuery::single(
        nmp_nip29::current_account_group_list_demand(),
    )));
    assert_eq!(kind_10009_atoms(&core.active_demand()), 0);

    let _ = core.handle(EngineMsg::SetActivePubkey(Some(a.public_key())));
    assert_eq!(
        kind_10009_atoms(&core.active_demand()),
        1,
        "signing in must reconstruct exactly one kind:10009 atom for the newly-current account"
    );
}

#[test]
fn rerooting_to_a_different_account_replaces_the_kind_10009_atom_not_adds_a_second() {
    let a = Keys::generate();
    let b = Keys::generate();
    let relay = RelayUrl::parse("wss://relay-a.example.com").unwrap();
    let dir = FixtureRoutingFacts::new()
        .with_outbound_routes(a.public_key(), [relay.clone()])
        .with_outbound_routes(b.public_key(), [relay]);
    let mut core = new_core(dir);

    let _ = core.handle(EngineMsg::Subscribe(LiveQuery::single(
        nmp_nip29::current_account_group_list_demand(),
    )));
    let _ = core.handle(EngineMsg::SetActivePubkey(Some(a.public_key())));
    assert_eq!(kind_10009_atoms(&core.active_demand()), 1);

    let _ = core.handle(EngineMsg::SetActivePubkey(Some(b.public_key())));
    let atoms = core.active_demand();
    assert_eq!(
        kind_10009_atoms(&atoms),
        1,
        "reroot must REPLACE the prior account's kind:10009 atom, never accumulate a second"
    );
    let atom = atoms
        .iter()
        .find(|a| a.filter.kinds.as_ref().is_some_and(|k| k.contains(&10009)))
        .expect("exactly one kind:10009 atom");
    assert_eq!(
        atom.filter.authors,
        Some(std::collections::BTreeSet::from([b.public_key().to_hex()])),
        "the surviving atom must resolve to the NEW current account, not the old one"
    );
}
