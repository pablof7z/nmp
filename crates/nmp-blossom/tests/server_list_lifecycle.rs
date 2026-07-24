//! BUD-03's live-query lifecycle falsifier (#731). The protocol crate only
//! supplies an ordinary reactive Demand; the resolver must reroot exactly
//! that demand on account change and must close before opening.

use std::collections::BTreeSet;

use nmp_blossom::{active_account_server_list_demand, USER_SERVER_LIST_KIND};
use nmp_grammar::{AccessContext, ConcreteFilter, ContextualAtom, DemandOp, SourceAuthority};
use nmp_resolver::testkit::Harness;
use nmp_resolver::LiveQuery;
use nostr::Keys;

fn atom(author: &str) -> ContextualAtom {
    ContextualAtom {
        filter: ConcreteFilter {
            kinds: Some(BTreeSet::from([USER_SERVER_LIST_KIND])),
            authors: Some(BTreeSet::from([author.to_string()])),
            ..ConcreteFilter::default()
        },
        source: SourceAuthority::AuthorOutboxes,
        access: AccessContext::Public,
        routing_evidence: BTreeSet::new(),
    }
}

#[test]
fn active_account_change_reroots_only_the_kind10063_demand_close_before_open() {
    let mut harness = Harness::new();
    let first = Keys::generate().public_key();
    let second = Keys::generate().public_key();
    harness.set_active(Some(first));

    let (_handle, opened) = harness.subscribe(LiveQuery(active_account_server_list_demand()));
    let first_atom = atom(&first.to_hex());
    assert_eq!(opened.ops, vec![DemandOp::Open(first_atom.clone())]);
    assert_eq!(
        harness.demand_with_context(),
        BTreeSet::from([first_atom.clone()])
    );

    let second_atom = atom(&second.to_hex());
    let reroot = harness.set_active(Some(second));
    assert_eq!(
        reroot.ops,
        vec![
            DemandOp::Close(first_atom),
            DemandOp::Open(second_atom.clone())
        ],
        "the old account closes before the new account opens, with no unrelated demand"
    );
    assert_eq!(
        harness.demand_with_context(),
        BTreeSet::from([second_atom.clone()])
    );

    let signed_out = harness.set_active(None);
    assert_eq!(signed_out.ops, vec![DemandOp::Close(second_atom)]);
    assert!(harness.demand_with_context().is_empty());
}
