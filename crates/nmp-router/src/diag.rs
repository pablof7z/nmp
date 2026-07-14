//! [`Diagnostics`] — the acceptance-test-made-visible, read-only projection
//! of a compiled plan (M2 plan §2.6): per-relay sub counts, lane counts,
//! reverse coverage (authors served), the exact filters sent, uncovered
//! authors, and dropped merge rules.

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::ConcreteFilter;

use crate::facts::{Lane, PubkeyHex, RelayUrl};
use crate::plan::RelayPlan;
use crate::solver::Shortfall;

#[derive(Clone, Debug)]
pub struct RelayDiagnostics {
    pub relay: RelayUrl,
    pub wire_sub_count: usize,
    pub by_lane: BTreeMap<Lane, usize>,
    /// Reverse coverage: distinct authors this relay covers.
    pub authors_served: usize,
    /// The EXACT filters sent to this relay.
    pub filters: Vec<ConcreteFilter>,
}

#[derive(Clone, Debug, Default)]
pub struct Diagnostics {
    pub per_relay: BTreeMap<RelayUrl, RelayDiagnostics>,
    pub uncovered_authors: BTreeMap<PubkeyHex, Shortfall>,
    /// Distinct candidates rejected by the one whole-demand relay ceiling.
    /// They are absent from `per_relay` by construction.
    pub relays_refused_by_cap: usize,
    pub dropped_merge_rules: Vec<&'static str>,
}

pub(crate) fn build(
    plan: &RelayPlan,
    uncovered_authors: BTreeMap<PubkeyHex, Shortfall>,
    dropped_merge_rules: Vec<&'static str>,
) -> Diagnostics {
    let mut per_relay = BTreeMap::new();
    for (relay, reqs) in &plan.reqs {
        let mut by_lane: BTreeMap<Lane, usize> = BTreeMap::new();
        let mut authors_served: BTreeSet<PubkeyHex> = BTreeSet::new();
        let mut filters = Vec::new();
        for req in reqs {
            filters.push(req.filter.clone());
            for prov in &req.provenance {
                *by_lane.entry(prov.lane).or_insert(0) += 1;
                authors_served.extend(prov.covers_authors.iter().cloned());
            }
        }
        per_relay.insert(
            relay.clone(),
            RelayDiagnostics {
                relay: relay.clone(),
                wire_sub_count: reqs.len(),
                by_lane,
                authors_served: authors_served.len(),
                filters,
            },
        );
    }
    Diagnostics {
        per_relay,
        uncovered_authors,
        relays_refused_by_cap: plan.refused_relays.len(),
        dropped_merge_rules,
    }
}
