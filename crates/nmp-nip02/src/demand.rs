use std::collections::BTreeSet;

use nmp_grammar::{Binding, Demand, Filter, IdentityField};

/// The active account's NIP-02 contact list through the ordinary reactive
/// live-query path. Logged out resolves to zero atoms; account changes
/// reroot this same demand without a component-managed subscription graph.
pub fn active_account_demand() -> Demand {
    Demand::from_filter(Filter {
        kinds: Some(BTreeSet::from([3u16])),
        authors: Some(Binding::Reactive(IdentityField::ActivePubkey)),
        ..Filter::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use nmp_grammar::SourceAuthority;

    #[test]
    fn contact_list_uses_active_account_author_outboxes() {
        let demand = active_account_demand();
        assert_eq!(demand.selection.kinds, Some(BTreeSet::from([3])));
        assert_eq!(
            demand.selection.authors,
            Some(Binding::Reactive(IdentityField::ActivePubkey))
        );
        assert_eq!(demand.source, SourceAuthority::AuthorOutboxes);
    }
}
