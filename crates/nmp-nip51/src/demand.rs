//! The active account's kind:10009 live-query constructor (#108).

use std::collections::BTreeSet;

use nmp_grammar::{Binding, Demand, Filter, IdentityField};

/// The signed-in account's remembered-groups list demand: `kinds:[10009],
/// authors: Reactive(ActivePubkey)`. `Demand::from_filter`'s existing
/// static default already resolves this to `AuthorOutboxes + Public`
/// (`Reactive` IS a bound `authors` field, same as any other `Binding`
/// variant) -- #108's issue text asks for exactly that default, so this
/// is `Demand::from_filter`, never an explicit `Demand::new` call.
///
/// Signed-out (no active pubkey) resolves this to zero atoms through the
/// ordinary `Reactive(ActivePubkey)` empty-resolution path (#106) -- no
/// special case needed here; see the `nmp-nip29` crate's own test proving
/// that reroot/reconstruct behavior end to end.
pub fn active_account_demand() -> Demand {
    Demand::from_filter(Filter {
        kinds: Some(BTreeSet::from([10009u16])),
        authors: Some(Binding::Reactive(IdentityField::ActivePubkey)),
        ..Filter::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use nmp_grammar::SourceAuthority;

    #[test]
    fn active_account_demand_uses_reactive_authors_and_author_outboxes_default() {
        let demand = active_account_demand();
        assert_eq!(demand.selection.kinds, Some(BTreeSet::from([10009u16])));
        assert_eq!(
            demand.selection.authors,
            Some(Binding::Reactive(IdentityField::ActivePubkey))
        );
        assert_eq!(demand.source, SourceAuthority::AuthorOutboxes);
    }
}
