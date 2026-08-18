//! Protocol-neutral route formation.

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::{
    ConcreteFilter, ReadRouting, RoutingEvidence, RoutingEvidenceKind,
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

}

/// Which routing path an atom takes through `compile`. There are exactly
/// two, and they are exactly [`ReadRouting`]'s two values — the class is
/// read off the atom's declared routing and nothing else.
///
/// In particular there is no longer a class that depends on the SHAPE of the
/// selection. `Auto` is one total path: the outbox solve, the projected
/// hint/provenance facts and the operator lanes all run for every `Auto`
/// atom, and a selection resolving no authors is that path's degenerate case
/// (empty candidates, empty solve, operator lanes carrying the whole route)
/// rather than a second class. A shape-derived class is precisely what the
/// deleted filter-only inference was; reintroducing one here would move that
/// inference rather than remove it.
#[derive(Debug)]
pub(crate) enum AtomClass {
    Auto,
    Exact(BTreeSet<RelayUrl>),
}

pub(crate) fn classify(routing: &ReadRouting) -> AtomClass {
    match routing {
        ReadRouting::Auto => AtomClass::Auto,
        ReadRouting::Explicit(relays) => AtomClass::Exact(relays.iter().cloned().collect()),
    }
}

/// The authors an atom's outbox lane will solve for: its resolved author
/// set under [`ReadRouting::Auto`], and empty under `Explicit`, which never
/// consults an outbox at all. Empty is also the honest answer for an `Auto`
/// atom whose selection names no author — it has no outbox work, only
/// operator lanes.
pub(crate) fn outbox_authors(atom: &ConcreteFilter, routing: &ReadRouting) -> BTreeSet<PublicKey> {
    match routing {
        ReadRouting::Auto => Skeleton::of(atom).1,
        ReadRouting::Explicit(_) => BTreeSet::new(),
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

    use nmp_router_testkit::test_relay;

    use super::*;
    use crate::facts::LocalFacts;
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
    fn explicit_routing_bypasses_fact_lookup() {
        let relay = test_relay(4);
        assert!(matches!(
            classify(&ReadRouting::Explicit(vec![relay])),
            AtomClass::Exact(_)
        ));
    }

    /// The classification is a function of the DECLARED routing alone. An
    /// author-bearing and an authorless selection classify identically under
    /// `Auto`, which is what makes `Auto` one path rather than a shape
    /// inference wearing a routing value's name.
    #[test]
    fn auto_classifies_the_same_whatever_the_selection_names() {
        let author = Keys::generate().public_key();
        assert!(matches!(classify(&ReadRouting::Auto), AtomClass::Auto));
        assert!(matches!(classify(&ReadRouting::Auto), AtomClass::Auto));
        // And the outbox author projection, which is a fact about the
        // selection rather than a class, still tells the two apart.
        assert_eq!(
            outbox_authors(&filter(author), &ReadRouting::Auto),
            BTreeSet::from([author])
        );
        assert!(outbox_authors(&ConcreteFilter::default(), &ReadRouting::Auto).is_empty());
    }

    /// `Explicit` never consults an outbox, so it contributes no outbox
    /// authors even over an author-bearing selection.
    #[test]
    fn explicit_routing_contributes_no_outbox_authors() {
        let author = Keys::generate().public_key();
        assert!(
            outbox_authors(&filter(author), &ReadRouting::Explicit(vec![test_relay(4)])).is_empty()
        );
    }

    #[test]
    fn present_outbound_becomes_neutral_coverage() {
        let author = Keys::generate().public_key();
        let facts = LocalFacts::new().with_author_routes(author, [test_relay(0)], []);
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
