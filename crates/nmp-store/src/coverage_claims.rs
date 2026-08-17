//! Canonical durable-coverage claim shapes.
//!
//! Relay routing may widen or synthesize a physical filter, but durable
//! coverage is always credited to these exact logical shapes. In particular,
//! one `Auto` atom spanning several authors owns one relay lifecycle while
//! proving one independently reusable coverage row per author.

use std::collections::BTreeSet;

use nmp_grammar::{ContextualAtom, ReadRouting};

/// Project one logical atom to the exact shapes it may claim durably.
///
/// This is the single normalization seam shared by the engine's attribution
/// registry and the router's immutable request payload. Callers retain the
/// original atom for demand ownership; only coverage claims are normalized.
pub fn coverage_claim_atoms(atom: &ContextualAtom) -> BTreeSet<ContextualAtom> {
    let ReadRouting::Auto = atom.routing else {
        // `Explicit` asked exactly one relay set for exactly this selection.
        // There is no per-author dimension to split along, so the atom is its
        // own claim whatever its authors say.
        return BTreeSet::from([atom.clone()]);
    };
    let Some(authors) = &atom.filter.authors else {
        // An `Auto` selection that names no author has no outbox dimension to
        // fan out over either: it is one exact claim over exactly what it
        // selected. Returning nothing here would mean an authorless live
        // query proves no durable coverage at all and re-fetches forever.
        return BTreeSet::from([atom.clone()]);
    };
    if authors.is_empty() {
        // Bound, but resolved to nobody. Nothing was asked of any author, so
        // nothing was proven about one — distinct from the unbound case above,
        // which asked about everyone.
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
    use nmp_grammar::ConcreteFilter;
    use nostr::RelayUrl;

    #[test]
    fn multi_author_auto_has_one_claim_shape_per_author() {
        let atom = ContextualAtom {
            filter: ConcreteFilter {
                kinds: Some(BTreeSet::from([0])),
                authors: Some(BTreeSet::from(["aa".repeat(32), "bb".repeat(32)])),
                since: Some(10),
                ..ConcreteFilter::default()
            },
            routing: ReadRouting::Auto,
            authenticate_as: None,
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

    /// An `Auto` selection naming NO author still claims coverage — one
    /// exact claim over what it selected. This is the case an authorless
    /// live query is, and dropping it would mean such a query proves nothing
    /// durable and re-fetches on every restart forever.
    #[test]
    fn an_authorless_auto_atom_remains_one_exact_claim_shape() {
        let atom = ContextualAtom {
            filter: ConcreteFilter::default(),
            routing: ReadRouting::Auto,
            authenticate_as: None,
            routing_evidence: BTreeSet::new(),
        };
        assert_eq!(coverage_claim_atoms(&atom), BTreeSet::from([atom]));
    }

    /// The one shape that owns no durable claim: `authors` BOUND but resolved
    /// to nobody. It is not the same as unbound — unbound asked about
    /// everyone and got an answer, this asked about nobody and got nothing.
    #[test]
    fn an_auto_atom_bound_to_no_author_owns_no_durable_claims() {
        let atom = ContextualAtom {
            filter: ConcreteFilter {
                authors: Some(BTreeSet::new()),
                ..ConcreteFilter::default()
            },
            routing: ReadRouting::Auto,
            authenticate_as: None,
            routing_evidence: BTreeSet::new(),
        };
        assert!(coverage_claim_atoms(&atom).is_empty());
    }

    /// `Explicit` never fans out, however many authors it names: it asked one
    /// exact relay set for one exact selection, and that is the only thing it
    /// proved. Only `Auto` splits per author, because only `Auto` chases each
    /// author's own outbox.
    #[test]
    fn an_explicit_author_set_is_not_normalized() {
        let explicit = ContextualAtom {
            filter: ConcreteFilter {
                authors: Some(BTreeSet::from(["aa".repeat(32), "bb".repeat(32)])),
                ..ConcreteFilter::default()
            },
            routing: ReadRouting::Explicit(vec![
                RelayUrl::parse("wss://coverage-claims.example").unwrap()
            ]),
            authenticate_as: None,
            routing_evidence: BTreeSet::new(),
        };
        assert_eq!(coverage_claim_atoms(&explicit), BTreeSet::from([explicit]));
    }
}
