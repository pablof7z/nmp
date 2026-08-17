//! The current account's kind:10009 Simple-groups-list query (#108/#1551).

use std::collections::BTreeSet;

use nmp_grammar::{Binding, Demand, Filter, IdentityField};

/// The signed-in account's remembered-groups list demand: `kinds:[10009],
/// authors: Reactive(ActivePubkey)`, read from the account's own outboxes
/// (#108's issue text asks for exactly that authority). `Reactive` IS a
/// bound `authors` field, same as any other `Binding` variant, so
/// `Demand::author_outboxes` accepts it.
///
/// Signed-out (no active pubkey) resolves this to zero atoms through the
/// ordinary `Reactive(ActivePubkey)` empty-resolution path (#106) -- no
/// special case needed here; `crates/nmp/tests/nip29_group_list_headless.rs` proves
/// that signed-out/reroot/reconstruct behavior end to end.
pub fn current_account_group_list_demand() -> Demand {
    Demand::author_outboxes(Filter {
        kinds: Some(BTreeSet::from([10009u16])),
        authors: Some(Binding::Reactive(IdentityField::ActivePubkey)),
        ..Filter::default()
    })
    .expect("the selection binds `authors`")
}

#[cfg(test)]
mod tests {
    use super::*;
    use nmp_grammar::SourceAuthority;

    #[test]
    fn current_account_demand_uses_reactive_authors_and_author_outboxes_default() {
        let demand = current_account_group_list_demand();
        assert_eq!(demand.selection.kinds, Some(BTreeSet::from([10009u16])));
        assert_eq!(
            demand.selection.authors,
            Some(Binding::Reactive(IdentityField::ActivePubkey))
        );
        assert_eq!(demand.source, SourceAuthority::AuthorOutboxes);
    }
}
