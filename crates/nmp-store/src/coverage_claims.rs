//! Canonical durable-coverage claim shapes.
//!
//! Relay routing may widen or synthesize a physical filter, but durable
//! coverage is always credited to these exact logical shapes. In
//! particular, one `AuthorOutboxes` atom spanning several authors owns one
//! relay lifecycle while proving one independently reusable coverage row per
//! author.

use std::collections::BTreeSet;

use nmp_grammar::{ContextualAtom, SourceAuthority};

/// Project one logical atom to the exact shapes it may claim durably.
///
/// This is the single normalization seam shared by the engine's attribution
/// registry and the router's immutable request payload. Callers retain the
/// original atom for demand ownership; only coverage claims are normalized.
pub fn coverage_claim_atoms(atom: &ContextualAtom) -> BTreeSet<ContextualAtom> {
    let SourceAuthority::AuthorOutboxes = atom.source else {
        return BTreeSet::from([atom.clone()]);
    };
    let Some(authors) = &atom.filter.authors else {
        return BTreeSet::new();
    };
    if authors.is_empty() {
        return BTreeSet::new();
    }
    if authors.len() == 1 {
        return BTreeSet::from([atom.clone()]);
    }

    authors
        .iter()
        .map(|author| {
            let mut claim = atom.clone();
            claim.filter.authors = Some(BTreeSet::from([author.clone()]));
            claim
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nmp_grammar::{AccessContext, ConcreteFilter};
    use nostr::RelayUrl;

    #[test]
    fn multi_author_outbox_has_one_claim_shape_per_author() {
        let atom = ContextualAtom {
            filter: ConcreteFilter {
                kinds: Some(BTreeSet::from([0])),
                authors: Some(BTreeSet::from(["aa".repeat(32), "bb".repeat(32)])),
                since: Some(10),
                ..ConcreteFilter::default()
            },
            source: SourceAuthority::AuthorOutboxes,
            access: AccessContext::Public,
            routing_evidence: BTreeSet::new(),
        };

        let claims = coverage_claim_atoms(&atom);
        assert_eq!(claims.len(), 2);
        assert!(claims.iter().all(|claim| claim
            .filter
            .authors
            .as_ref()
            .is_some_and(|authors| authors.len() == 1)));
        assert!(claims.iter().all(|claim| claim.filter.since == Some(10)));
    }

    #[test]
    fn non_outbox_atom_remains_one_exact_claim_shape() {
        let atom = ContextualAtom {
            filter: ConcreteFilter::default(),
            source: SourceAuthority::Public,
            access: AccessContext::Public,
            routing_evidence: BTreeSet::new(),
        };
        assert_eq!(coverage_claim_atoms(&atom), BTreeSet::from([atom]));
    }

    #[test]
    fn invalid_author_outbox_shapes_own_no_durable_claims() {
        let atom = ContextualAtom {
            filter: ConcreteFilter::default(),
            source: SourceAuthority::AuthorOutboxes,
            access: AccessContext::Public,
            routing_evidence: BTreeSet::new(),
        };
        assert!(coverage_claim_atoms(&atom).is_empty());

        let mut empty = atom;
        empty.filter.authors = Some(BTreeSet::new());
        assert!(coverage_claim_atoms(&empty).is_empty());
    }

    #[test]
    fn public_and_pinned_author_sets_are_not_normalized() {
        let filter = ConcreteFilter {
            authors: Some(BTreeSet::from(["aa".repeat(32), "bb".repeat(32)])),
            ..ConcreteFilter::default()
        };
        let public = ContextualAtom {
            filter: filter.clone(),
            source: SourceAuthority::Public,
            access: AccessContext::Public,
            routing_evidence: BTreeSet::new(),
        };
        assert_eq!(coverage_claim_atoms(&public), BTreeSet::from([public]));

        let pinned = ContextualAtom {
            filter,
            source: SourceAuthority::Pinned(BTreeSet::from([RelayUrl::parse(
                "wss://coverage-claims.example",
            )
            .unwrap()])),
            access: AccessContext::Public,
            routing_evidence: BTreeSet::new(),
        };
        assert_eq!(coverage_claim_atoms(&pinned), BTreeSet::from([pinned]));
    }
}
