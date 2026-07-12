//! #108: `nmp_nip51::active_account_demand()` proven against a REAL
//! `EngineCore` -- signed-out state yields no active-account kind-10009
//! demand, and signing in (or rerooting to a different account)
//! reconstructs it correctly. This is a cross-crate integration proof
//! `nmp-nip51` itself cannot make (it is deliberately engine-free).

use std::sync::{Arc, Mutex};

use nmp_engine::core::{EngineCore, EngineMsg, RowDelta, RowSink};
use nmp_grammar::ContextualAtom;
use nmp_resolver::LiveQuery;
use nmp_router::FixtureDirectory;
use nmp_store::MemoryStore;
use nostr::{Keys, RelayUrl};

#[derive(Clone, Default)]
struct CapturingSink(Arc<Mutex<Vec<Vec<RowDelta>>>>);

impl RowSink for CapturingSink {
    fn on_rows(&self, rows: Vec<RowDelta>) {
        self.0.lock().unwrap().push(rows);
    }
}

fn new_core(dir: FixtureDirectory) -> EngineCore<MemoryStore> {
    EngineCore::new(MemoryStore::new(), Box::new(dir), 10)
}

fn kind_10009_atoms(atoms: &std::collections::BTreeSet<ContextualAtom>) -> usize {
    atoms
        .iter()
        .filter(|a| a.filter.kinds.as_ref().is_some_and(|k| k.contains(&10009)))
        .count()
}

#[test]
fn signed_out_active_account_demand_resolves_to_zero_atoms() {
    let mut core = new_core(FixtureDirectory::new());
    let _ = core.handle(EngineMsg::Subscribe(
        LiveQuery(nmp_nip51::active_account_demand()),
        Box::new(CapturingSink::default()),
    ));
    assert_eq!(
        kind_10009_atoms(&core.active_demand()),
        0,
        "signed-out (no active pubkey) must yield zero kind:10009 atoms, never a \
         fabricated/empty-author subscription"
    );
}

#[test]
fn signing_in_reconstructs_the_active_account_kind_10009_demand() {
    let a = Keys::generate();
    let relay = RelayUrl::parse("wss://relay-a.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay]);
    let mut core = new_core(dir);

    let _ = core.handle(EngineMsg::Subscribe(
        LiveQuery(nmp_nip51::active_account_demand()),
        Box::new(CapturingSink::default()),
    ));
    assert_eq!(kind_10009_atoms(&core.active_demand()), 0);

    let _ = core.handle(EngineMsg::SetActivePubkey(Some(a.public_key())));
    assert_eq!(
        kind_10009_atoms(&core.active_demand()),
        1,
        "signing in must reconstruct exactly one kind:10009 atom for the newly-active account"
    );
}

#[test]
fn rerooting_to_a_different_account_replaces_the_kind_10009_atom_not_adds_a_second() {
    let a = Keys::generate();
    let b = Keys::generate();
    let relay = RelayUrl::parse("wss://relay-a.example.com").unwrap();
    let dir = FixtureDirectory::new()
        .with_write(a.public_key().to_hex(), [relay.clone()])
        .with_write(b.public_key().to_hex(), [relay]);
    let mut core = new_core(dir);

    let _ = core.handle(EngineMsg::Subscribe(
        LiveQuery(nmp_nip51::active_account_demand()),
        Box::new(CapturingSink::default()),
    ));
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
        "the surviving atom must resolve to the NEW active account, not the old one"
    );
}
