//! Atom classification (outbox vs pinned), the [`Skeleton`] key, candidate
//! relay-list assembly (lane-ordered, discovery-kind indexer eligibility),
//! and pinned-route lookup (M2 plan §2.2, §3, §4.1 steps 1-2).

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::{ConcreteFilter, DescriptorHash};

use crate::facts::{DiscoveryKinds, Lane, LanedRelay, PubkeyHex, RelayDirectory, RelayUrl};
use crate::solver::Coverage;

/// Why one relay is in the plan for one atom — typed provenance (ledger
/// #3/#4: "every explicit route carries typed provenance"; "no connection
/// outside a solver-produced plan"). Every wire REQ traces back to one or
/// more of these.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RouteProvenance {
    pub relay: RelayUrl,
    pub lane: Lane,
    /// Which authors of the atom this relay covers (outbox), or empty for a
    /// pinned non-author route.
    pub covers_authors: BTreeSet<PubkeyHex>,
    /// Solver-produced (outbox) vs pinned-fact (group host / dm inbox).
    pub route_kind: RouteKind,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RouteKind {
    OutboxSolved,
    Pinned,
}

/// A demand atom with its routable (author) dimension projected OUT.
/// Atoms that share a skeleton are coverage-solved TOGETHER so their
/// covering relay set is shared (and their per-relay atoms re-coalesce,
/// §4). The skeleton is also the coalescing / sub-id key: two atoms have
/// the same `Skeleton` iff they are identical except (possibly) `authors`.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Skeleton(ConcreteFilter);

impl Skeleton {
    /// Erase `atom`'s `authors` field, returning the skeleton and the
    /// (possibly empty) author set that was erased. A non-author (pinned)
    /// atom already has `authors: None`, so this erases nothing for it and
    /// returns an empty author set — `Skeleton::of` is therefore total over
    /// both atom classes.
    pub fn of(atom: &ConcreteFilter) -> (Skeleton, BTreeSet<PubkeyHex>) {
        let authors = atom.authors.clone().unwrap_or_default();
        let mut base = atom.clone();
        base.authors = None;
        (Skeleton(base), authors)
    }

    /// Rebuild a concrete filter for this skeleton with `authors`
    /// materialized (`None` if empty, matching how atoms without an author
    /// dimension are represented).
    pub fn with_authors(&self, authors: BTreeSet<PubkeyHex>) -> ConcreteFilter {
        let mut cf = self.0.clone();
        cf.authors = if authors.is_empty() {
            None
        } else {
            Some(authors)
        };
        cf
    }

    pub fn kinds(&self) -> &Option<BTreeSet<u16>> {
        &self.0.kinds
    }

    pub fn hash(&self) -> DescriptorHash {
        self.0.hash()
    }
}

/// The classification of a demand atom before routing.
pub(crate) enum AtomClass {
    /// Author-bearing: coverage-solve the author set.
    Outbox {
        skeleton: Skeleton,
        authors: BTreeSet<PubkeyHex>,
    },
    /// No authors: relays come directly from a lane fact (pinned).
    Pinned,
}

pub(crate) fn classify(atom: &ConcreteFilter) -> AtomClass {
    let (skeleton, authors) = Skeleton::of(atom);
    if authors.is_empty() {
        AtomClass::Pinned
    } else {
        AtomClass::Outbox { skeleton, authors }
    }
}

/// Build the per-author candidate relay list (lane-ordered: `write_relays`
/// -- Nip65Write -- first, then `extra_relays`; indexer relays appended
/// ONLY when `skeleton` is discovery-kind, never for content atoms).
pub(crate) fn build_candidates(
    authors: &BTreeSet<PubkeyHex>,
    dir: &dyn RelayDirectory,
    discovery: &DiscoveryKinds,
    skeleton: &Skeleton,
) -> (BTreeMap<PubkeyHex, Vec<LanedRelay>>, Vec<RelayUrl>) {
    let is_discovery = discovery.is_discovery(skeleton.kinds());
    let indexer_relays: Vec<RelayUrl> = if is_discovery {
        dir.indexers()
    } else {
        Vec::new()
    };

    let mut candidates = BTreeMap::new();
    for author in authors {
        let mut list = dir.write_relays(author);
        list.extend(dir.extra_relays(author));
        if is_discovery {
            list.extend(
                indexer_relays
                    .iter()
                    .cloned()
                    .map(|url| LanedRelay::new(url, Lane::IndexerDiscovery)),
            );
        }
        candidates.insert(author.clone(), list);
    }
    (candidates, indexer_relays)
}

/// The lane that supplied `relay` for `author`, per `candidates` (first
/// match wins — `write_relays` is listed before `extra_relays`/indexers in
/// `build_candidates`, so ties prefer the higher-priority lane).
pub(crate) fn lane_of(
    candidates: &BTreeMap<PubkeyHex, Vec<LanedRelay>>,
    author: &PubkeyHex,
    relay: &RelayUrl,
) -> Lane {
    candidates
        .get(author)
        .and_then(|list| list.iter().find(|lr| &lr.url == relay))
        .map(|lr| lr.lane)
        .expect("solver-assigned relay must be one of the author's own candidates")
}

/// Turn a solved [`Coverage`]'s per-author assignment into one
/// `RouteProvenance` per (author, relay) pair -- deliberately NOT grouped
/// or unioned here. Author-union is achieved entirely downstream, by
/// `coalesce::RuleRegistry`'s `AuthorUnion` rule folding these single-author
/// entries together (M2 plan §4.1 step 4) -- the SAME real mechanism the
/// widen-only property test proves and the kill measurement's "dedup-only
/// vs with-AuthorUnion" tiers toggle by registry choice alone. Mirrors
/// [`provenance_for_pinned`]'s shape.
pub(crate) fn provenance_for_outbox(
    coverage: &Coverage,
    candidates: &BTreeMap<PubkeyHex, Vec<LanedRelay>>,
) -> Vec<(RelayUrl, RouteProvenance)> {
    let mut out = Vec::new();
    for (author, relays) in &coverage.assignment {
        for relay in relays {
            let lane = lane_of(candidates, author, relay);
            out.push((
                relay.clone(),
                RouteProvenance {
                    relay: relay.clone(),
                    lane,
                    covers_authors: BTreeSet::from([author.clone()]),
                    route_kind: RouteKind::OutboxSolved,
                },
            ));
        }
    }
    out
}

/// Pinned-route lookup for a non-author atom: every relay the directory
/// returns becomes a `RouteProvenance` with `route_kind: Pinned` and no
/// covered authors.
pub(crate) fn provenance_for_pinned(
    atom: &ConcreteFilter,
    dir: &dyn RelayDirectory,
) -> Vec<(RelayUrl, RouteProvenance)> {
    dir.pinned_relays(atom)
        .into_iter()
        .map(|lr| {
            (
                lr.url.clone(),
                RouteProvenance {
                    relay: lr.url,
                    lane: lr.lane,
                    covers_authors: BTreeSet::new(),
                    route_kind: RouteKind::Pinned,
                },
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::{test_relay, FixtureDirectory};
    use crate::solver::{solve, CoverageInput};

    fn pk(c: char) -> PubkeyHex {
        c.to_string().repeat(64)
    }

    fn cf_kind1(authors: Option<BTreeSet<PubkeyHex>>) -> ConcreteFilter {
        ConcreteFilter {
            kinds: Some(BTreeSet::from([1u16])),
            authors,
            ..ConcreteFilter::default()
        }
    }

    #[test]
    fn skeleton_of_erases_authors_and_reconstructs() {
        let atom = cf_kind1(Some(BTreeSet::from([pk('a')])));
        let (skeleton, authors) = Skeleton::of(&atom);
        assert_eq!(authors, BTreeSet::from([pk('a')]));
        assert_eq!(skeleton.with_authors(authors), atom);
    }

    #[test]
    fn skeleton_of_pinned_atom_has_empty_authors() {
        let atom = cf_kind1(None);
        let (skeleton, authors) = Skeleton::of(&atom);
        assert!(authors.is_empty());
        assert_eq!(skeleton.with_authors(BTreeSet::new()), atom);
    }

    #[test]
    fn classify_distinguishes_outbox_and_pinned() {
        assert!(matches!(
            classify(&cf_kind1(Some(BTreeSet::from([pk('a')])))),
            AtomClass::Outbox { .. }
        ));
        assert!(matches!(classify(&cf_kind1(None)), AtomClass::Pinned));
    }

    #[test]
    fn indexer_candidates_only_for_discovery_kinds() {
        let dir = FixtureDirectory::new().with_indexer(test_relay(99));
        let discovery = DiscoveryKinds::default();

        let content_atom = cf_kind1(Some(BTreeSet::from([pk('a')])));
        let (content_skeleton, content_authors) = Skeleton::of(&content_atom);
        let (content_candidates, content_indexers) =
            build_candidates(&content_authors, &dir, &discovery, &content_skeleton);
        assert!(content_indexers.is_empty());
        assert!(content_candidates[&pk('a')].is_empty());

        let discovery_atom = ConcreteFilter {
            kinds: Some(BTreeSet::from([3u16])),
            authors: Some(BTreeSet::from([pk('a')])),
            ..ConcreteFilter::default()
        };
        let (discovery_skeleton, discovery_authors) = Skeleton::of(&discovery_atom);
        let (discovery_candidates, discovery_indexers) =
            build_candidates(&discovery_authors, &dir, &discovery, &discovery_skeleton);
        assert_eq!(discovery_indexers, vec![test_relay(99)]);
        assert!(discovery_candidates[&pk('a')]
            .iter()
            .any(|lr| lr.lane == Lane::IndexerDiscovery));
    }

    #[test]
    fn provenance_for_outbox_yields_one_entry_per_author_relay_pair() {
        let dir = FixtureDirectory::new().with_write(pk('a'), [test_relay(0), test_relay(1)]);
        let discovery = DiscoveryKinds::default();
        let atom = cf_kind1(Some(BTreeSet::from([pk('a')])));
        let (skeleton, authors) = Skeleton::of(&atom);
        let (candidates, indexer_relays) = build_candidates(&authors, &dir, &discovery, &skeleton);
        let coverage = solve(&CoverageInput {
            candidates: candidates.clone(),
            k: 2,
            cap: 10,
            indexer_eligible_relays: indexer_relays,
        });
        let provenance = provenance_for_outbox(&coverage, &candidates);
        assert_eq!(
            provenance.len(),
            2,
            "one entry per (author, relay) pair, un-grouped"
        );
        for (_, prov) in provenance {
            assert_eq!(prov.lane, Lane::Nip65Write);
            assert_eq!(prov.covers_authors, BTreeSet::from([pk('a')]));
            assert_eq!(prov.route_kind, RouteKind::OutboxSolved);
        }
    }
}
