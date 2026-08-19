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

