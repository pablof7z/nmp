//! Protocol-neutral route formation.

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::{
    ConcreteFilter, DescriptorHash, RoutingEvidence, RoutingEvidenceKind, SourceAuthority,
};

use crate::facts::{AuthorRouteState, Lane, LanedRelay, PublicKey, RelayUrl, RoutingFacts};
use crate::solver::Coverage;

/// Why one relay is in the plan for one atom.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct RouteProvenance {
    pub relay: RelayUrl,
    pub lane: Lane,
    pub covers_authors: BTreeSet<PublicKey>,
    pub route_kind: RouteKind,
}

/// How a route was formed, independent of the protocol that requested it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum RouteKind {
    Coverage,
    Supplemental,
    Exact,
}

/// A demand atom with its routable author dimension projected out.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Skeleton(ConcreteFilter);

impl Skeleton {
    pub fn of(atom: &ConcreteFilter) -> (Skeleton, BTreeSet<PublicKey>) {
        let authors = atom
            .authors
            .as_ref()
            .into_iter()
            .flatten()
            .map(|author| {
                PublicKey::from_hex(author)
                    .expect("resolved ConcreteFilter authors are validated public keys")
            })
            .collect();
        let mut base = atom.clone();
        base.authors = None;
        (Skeleton(base), authors)
    }

    /// Materialize decoded internal keys only at the wire-filter boundary.
    pub fn with_authors(&self, authors: BTreeSet<PublicKey>) -> ConcreteFilter {
        let mut filter = self.0.clone();
        filter.authors = if authors.is_empty() {
            None
        } else {
            Some(authors.into_iter().map(|author| author.to_hex()).collect())
        };
        filter
    }

    pub fn hash(&self) -> DescriptorHash {
        self.0.hash()
    }
}

#[derive(Debug)]
pub(crate) enum AtomClass {
    Coverage {
        skeleton: Skeleton,
        authors: BTreeSet<PublicKey>,
    },
    Supplemental {
        authors: BTreeSet<PublicKey>,
    },
    Exact(BTreeSet<RelayUrl>),
}

pub(crate) fn classify(atom: &ConcreteFilter, source: &SourceAuthority) -> AtomClass {
    match source {
        SourceAuthority::AuthorOutboxes => {
            let (skeleton, authors) = Skeleton::of(atom);
            AtomClass::Coverage { skeleton, authors }
        }
        SourceAuthority::Public => {
            let (_, authors) = Skeleton::of(atom);
            AtomClass::Supplemental { authors }
        }
        SourceAuthority::Pinned(relays) => AtomClass::Exact(relays.clone()),
    }
}

/// Build the authoritative outbound candidate list. Unknown and absent facts
/// both contribute no relays; their distinction remains available to the
/// write resolver rather than being guessed by the router.
pub(crate) fn build_candidates(
    authors: &BTreeSet<PublicKey>,
    facts: &dyn RoutingFacts,
) -> BTreeMap<PublicKey, Vec<LanedRelay>> {
    authors
        .iter()
        .map(|author| {
            let relays = match facts.author_routes(author) {
                AuthorRouteState::Present(routes) => routes
                    .outbound()
                    .iter()
                    .cloned()
                    .map(|url| LanedRelay::new(url, Lane::AuthorOutbound))
                    .collect(),
                AuthorRouteState::Unknown | AuthorRouteState::Absent => Vec::new(),
            };
            (*author, relays)
        })
        .collect()
}

/// Add selector-projected hint/provenance facts to the coverage candidates.
pub(crate) fn add_projected_candidates(
    candidates: &mut BTreeMap<PublicKey, Vec<LanedRelay>>,
    evidence_by_author: &BTreeMap<PublicKey, BTreeSet<RoutingEvidence>>,
) {
    for (author, evidence) in evidence_by_author {
        let list = candidates.entry(*author).or_default();
        for fact in evidence {
            if list.iter().any(|candidate| candidate.url == fact.relay) {
                continue;
            }
            list.push(LanedRelay::new(
                fact.relay.clone(),
                match fact.origin {
                    RoutingEvidenceKind::Hint => Lane::Hint,
                    RoutingEvidenceKind::SourceProvenance => Lane::Provenance,
                },
            ));
        }
    }
}

pub(crate) fn provenance_for_projected(
    evidence: &BTreeSet<RoutingEvidence>,
) -> Vec<(RelayUrl, RouteProvenance)> {
    evidence
        .iter()
        .map(|fact| {
            (
                fact.relay.clone(),
                RouteProvenance {
                    relay: fact.relay.clone(),
                    lane: match fact.origin {
                        RoutingEvidenceKind::Hint => Lane::Hint,
                        RoutingEvidenceKind::SourceProvenance => Lane::Provenance,
                    },
                    covers_authors: BTreeSet::new(),
                    route_kind: RouteKind::Supplemental,
                },
            )
        })
        .collect()
}

pub(crate) fn operator_app_routes(
    facts: &dyn RoutingFacts,
    covers_authors: &BTreeSet<PublicKey>,
) -> Vec<(RelayUrl, RouteProvenance)> {
    facts
        .operator_app_relays()
        .into_iter()
        .map(|relay| {
            (
                relay.clone(),
                RouteProvenance {
                    relay,
                    lane: Lane::OperatorApp,
                    covers_authors: covers_authors.clone(),
                    route_kind: RouteKind::Supplemental,
                },
            )
        })
        .collect()
}

pub(crate) fn operator_fallback_routes(
    facts: &dyn RoutingFacts,
    shortfall_authors: &BTreeSet<PublicKey>,
) -> Vec<(RelayUrl, RouteProvenance)> {
    if shortfall_authors.is_empty() || !facts.operator_app_relays().is_empty() {
        return Vec::new();
    }
    facts
        .operator_fallback_relays()
        .into_iter()
        .map(|relay| {
            (
                relay.clone(),
                RouteProvenance {
                    relay,
                    lane: Lane::OperatorFallback,
                    covers_authors: shortfall_authors.clone(),
                    route_kind: RouteKind::Supplemental,
                },
            )
        })
        .collect()
}

pub(crate) fn lane_of(
    candidates: &BTreeMap<PublicKey, Vec<LanedRelay>>,
    author: &PublicKey,
    relay: &RelayUrl,
) -> Lane {
    candidates
        .get(author)
        .and_then(|list| list.iter().find(|candidate| &candidate.url == relay))
        .map(|candidate| candidate.lane)
        .expect("solver-assigned relay must be one of the author's candidates")
}

pub(crate) fn provenance_for_coverage(
    coverage: &Coverage,
    candidates: &BTreeMap<PublicKey, Vec<LanedRelay>>,
) -> Vec<(RelayUrl, RouteProvenance)> {
    let mut routes = Vec::new();
    for (author, relays) in &coverage.assignment {
        for relay in relays {
            routes.push((
                relay.clone(),
                RouteProvenance {
                    relay: relay.clone(),
                    lane: lane_of(candidates, author, relay),
                    covers_authors: BTreeSet::from([*author]),
                    route_kind: RouteKind::Coverage,
                },
            ));
        }
    }
    routes
}

pub(crate) fn provenance_for_exact(
    relays: &BTreeSet<RelayUrl>,
) -> Vec<(RelayUrl, RouteProvenance)> {
    relays
        .iter()
        .map(|relay| {
            (
                relay.clone(),
                RouteProvenance {
                    relay: relay.clone(),
                    lane: Lane::Exact,
                    covers_authors: BTreeSet::new(),
                    route_kind: RouteKind::Exact,
                },
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use nostr::Keys;

    use super::*;
    use crate::facts::{test_relay, FixtureRoutingFacts};
    use crate::solver::{solve, CoverageInput};

    fn filter(author: PublicKey) -> ConcreteFilter {
        ConcreteFilter {
            kinds: Some(BTreeSet::from([1])),
            authors: Some(BTreeSet::from([author.to_hex()])),
            ..ConcreteFilter::default()
        }
    }

    #[test]
    fn skeleton_converts_only_at_filter_boundary() {
        let author = Keys::generate().public_key();
        let atom = filter(author);
        let (skeleton, authors) = Skeleton::of(&atom);
        assert_eq!(authors, BTreeSet::from([author]));
        assert_eq!(skeleton.with_authors(authors), atom);
    }

    #[test]
    fn exact_source_bypasses_fact_lookup() {
        let relay = test_relay(4);
        assert!(matches!(
            classify(
                &ConcreteFilter::default(),
                &SourceAuthority::Pinned(BTreeSet::from([relay]))
            ),
            AtomClass::Exact(_)
        ));
    }

    #[test]
    fn present_outbound_becomes_neutral_coverage() {
        let author = Keys::generate().public_key();
        let facts = FixtureRoutingFacts::new().with_author_routes(author, [test_relay(0)], []);
        let candidates = build_candidates(&BTreeSet::from([author]), &facts);
        let coverage = solve(&CoverageInput {
            candidates: candidates.clone(),
            k: 1,
            cap: 1,
        });
        let route = provenance_for_coverage(&coverage, &candidates)
            .into_iter()
            .next()
            .expect("route");
        assert_eq!(route.1.lane, Lane::AuthorOutbound);
        assert_eq!(route.1.route_kind, RouteKind::Coverage);
    }
}
