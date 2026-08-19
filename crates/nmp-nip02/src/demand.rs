use std::collections::BTreeSet;

use nmp_grammar::{Binding, Demand, Filter, IdentityField};

/// The current account's NIP-02 contact list through the ordinary reactive
/// live-query path. Logged out resolves to zero atoms; account changes
/// reroot this same demand without a component-managed subscription graph.
pub fn current_account_demand() -> Demand {
    Demand {
        selection: Filter {
            kinds: Some(BTreeSet::from([3u16])),
            authors: Some(Binding::Reactive(IdentityField::ActivePubkey)),
            ..Filter::default()
        },
        ..Demand::default()
    }
}

