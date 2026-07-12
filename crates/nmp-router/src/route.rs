//! Atom classification (outbox vs pinned), the [`Skeleton`] key, own-relay
//! candidate assembly for the coverage solver, the additive indexer/app/
//! fallback lane routes applied outside the solve (Unit B,
//! `routing-and-ownership.md` §2.1/§2.2), and pinned-route lookup (M2 plan
//! §2.2, §3, §4.1 steps 1-2).

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::{ConcreteFilter, DescriptorHash, SourceAuthority};

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
    /// Query-declared (#107) -- routed directly to the Demand's own relay
    /// set, no directory lookup, no additive lane applied alongside it.
    ExplicitPinned,
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
    /// Explicit, query-declared pinned wire authority (#107,
    /// `SourceAuthority::Pinned`): route ONLY to this relay set, bypassing
    /// the outbox solve, the directory pinned-lookup, AND every additive
    /// lane (indexer/app/fallback) entirely -- regardless of whether the
    /// selection is author-bearing.
    ExplicitPinned(BTreeSet<RelayUrl>),
}

/// Classify a demand atom by its DECLARED [`SourceAuthority`] (#106), never
/// by incidentally inferring routing intent from whether `atom.authors`
/// happens to be populated. Before #106, `authors.is_empty()` alone decided
/// Outbox vs Pinned; that inference is byte-identical to this for every
/// atom `Demand::from_filter`'s static default produces (an `AuthorOutboxes`
/// atom's `authors` is never empty in practice — an empty-resolving authors
/// binding yields zero atoms at all, never a materialized atom with empty
/// `authors`), so today's regression floor holds unchanged. The seam this
/// opens: a caller MAY construct a `Demand` with `source: Public` even when
/// its selection constrains `authors` (e.g. NIP-29-tagged content routed via
/// the group host, not each author's own outbox) — SourceAuthority, not
/// filter shape, is now authoritative.
pub(crate) fn classify(atom: &ConcreteFilter, source: &SourceAuthority) -> AtomClass {
    match source {
        SourceAuthority::AuthorOutboxes => {
            let (skeleton, authors) = Skeleton::of(atom);
            AtomClass::Outbox { skeleton, authors }
        }
        SourceAuthority::Public => AtomClass::Pinned,
        SourceAuthority::Pinned(relays) => AtomClass::ExplicitPinned(relays.clone()),
    }
}

/// Build the per-author candidate relay list for the coverage solver — the
/// author's OWN relays ONLY, the set that counts toward `k`
/// (`routing-and-ownership.md` §2.1, owner-resolved §9-decision-3 /
/// `routing-build-plan.md` §7.1 Q3): `write_relays` (`Nip65Write`) first,
/// then `extra_relays` filtered to `Hint`/`Provenance` lanes (relay hints —
/// both write- and read-side — count toward the minimum; `UserConfigured`
/// extras do not). Indexer/app/fallback relays are NEVER folded in here —
/// they are additive lanes applied OUTSIDE the solve, in `Router::compile`
/// (`indexer_lane_routes`/`app_lane_routes`/`fallback_lane_routes` below).
pub(crate) fn build_candidates(
    authors: &BTreeSet<PubkeyHex>,
    dir: &dyn RelayDirectory,
) -> BTreeMap<PubkeyHex, Vec<LanedRelay>> {
    let mut candidates = BTreeMap::new();
    for author in authors {
        let mut list = dir.write_relays(author);
        list.extend(
            dir.extra_relays(author)
                .into_iter()
                .filter(|lr| matches!(lr.lane, Lane::Hint | Lane::Provenance)),
        );
        candidates.insert(author.clone(), list);
    }
    candidates
}

/// Additive indexer-lane routes for an outbox group: every `dir.indexers()`
/// relay, unconditional, covering the group's FULL author set — but ONLY
/// when `skeleton` is discovery-kind (indexers are never a content
/// fallback). Applied OUTSIDE the solve; never counted toward `k`
/// (`routing-and-ownership.md` §2.1/§2.2 item 1).
pub(crate) fn indexer_lane_routes(
    dir: &dyn RelayDirectory,
    discovery: &DiscoveryKinds,
    skeleton: &Skeleton,
    authors: &BTreeSet<PubkeyHex>,
) -> Vec<(RelayUrl, RouteProvenance)> {
    if !discovery.is_discovery(skeleton.kinds()) {
        return Vec::new();
    }
    dir.indexers()
        .into_iter()
        .map(|relay| {
            (
                relay.clone(),
                RouteProvenance {
                    relay,
                    lane: Lane::IndexerDiscovery,
                    covers_authors: authors.clone(),
                    route_kind: RouteKind::Pinned,
                },
            )
        })
        .collect()
}

/// Additive app-lane routes: every `dir.app_relays()` relay, unconditional,
/// for ANY atom — author-bearing (`covers_authors` = the atom's authors) or
/// authorless/pinned (`covers_authors` empty). Every kind, every author,
/// always (this is what closes #7, the authorless-routing-lane gap).
/// Applied OUTSIDE the solve; never counted toward `k`
/// (`routing-and-ownership.md` §2.1/§2.2 item 2).
pub(crate) fn app_lane_routes(
    dir: &dyn RelayDirectory,
    covers_authors: &BTreeSet<PubkeyHex>,
) -> Vec<(RelayUrl, RouteProvenance)> {
    dir.app_relays()
        .into_iter()
        .map(|relay| {
            (
                relay.clone(),
                RouteProvenance {
                    relay,
                    lane: Lane::AppRelay,
                    covers_authors: covers_authors.clone(),
                    route_kind: RouteKind::Pinned,
                },
            )
        })
        .collect()
}

/// Additive fallback-lane routes: every `dir.fallback_relays()` relay,
/// routing exactly `shortfall_authors` (the outbox solve's own-relay
/// coverage `< k` set, `Coverage.shortfall`) — fires ONLY when
/// `shortfall_authors` is non-empty AND no `app_relays` are configured
/// (an `AppRelay` suppresses fallback entirely). `Coverage.shortfall` stays
/// REPORTED even when this lane tops an author up — fallback is a lane,
/// not coverage (`routing-and-ownership.md` §2.1/§2.2 item 5).
pub(crate) fn fallback_lane_routes(
    dir: &dyn RelayDirectory,
    shortfall_authors: &BTreeSet<PubkeyHex>,
) -> Vec<(RelayUrl, RouteProvenance)> {
    if shortfall_authors.is_empty() || !dir.app_relays().is_empty() {
        return Vec::new();
    }
    dir.fallback_relays()
        .into_iter()
        .map(|relay| {
            (
                relay.clone(),
                RouteProvenance {
                    relay,
                    lane: Lane::Fallback,
                    covers_authors: shortfall_authors.clone(),
                    route_kind: RouteKind::Pinned,
                },
            )
        })
        .collect()
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

/// Explicit pinned-route lookup (#107): route DIRECTLY to the Demand's own
/// declared relay set -- no directory lookup, mirroring
/// [`provenance_for_pinned`]'s shape but sourcing relays from
/// `SourceAuthority::Pinned`'s own payload instead of a fixture fact. Callers
/// MUST NOT layer any additive lane (indexer/app/fallback) on top of this
/// route set -- that's the #107 Contract's core guarantee, enforced at the
/// `Router::compile` call site by routing `AtomClass::ExplicitPinned` through
/// a dedicated step that never touches `indexer_lane_routes`/
/// `app_lane_routes`/`fallback_lane_routes`.
pub(crate) fn provenance_for_explicit_pinned(
    relays: &BTreeSet<RelayUrl>,
) -> Vec<(RelayUrl, RouteProvenance)> {
    relays
        .iter()
        .map(|relay| {
            (
                relay.clone(),
                RouteProvenance {
                    relay: relay.clone(),
                    lane: Lane::ExplicitPinned,
                    covers_authors: BTreeSet::new(),
                    route_kind: RouteKind::ExplicitPinned,
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
            classify(
                &cf_kind1(Some(BTreeSet::from([pk('a')]))),
                &SourceAuthority::AuthorOutboxes
            ),
            AtomClass::Outbox { .. }
        ));
        assert!(matches!(
            classify(&cf_kind1(None), &SourceAuthority::Public),
            AtomClass::Pinned
        ));
    }

    /// The seam #106 opens: DECLARED `SourceAuthority` decides, not filter
    /// shape -- an author-bearing atom explicitly declared `Public` routes
    /// Pinned (e.g. NIP-29-tagged content routed via the group host, not
    /// each author's own outbox).
    #[test]
    fn classify_honors_declared_source_over_filter_shape() {
        let author_bearing = cf_kind1(Some(BTreeSet::from([pk('a')])));
        assert!(matches!(
            classify(&author_bearing, &SourceAuthority::Public),
            AtomClass::Pinned
        ));
    }

    /// `build_candidates` no longer folds indexers into the per-author
    /// candidate list at all (Unit B moved the indexer lane OUTSIDE the
    /// solve, into `Router::compile` — see `indexer_lane_routes` and the
    /// router-level `indexer_lane_still_discovery_only_never_content_fallback`
    /// regression test, which re-asserts this invariant survives the move).
    /// This test pins the narrower claim at this layer: candidates are the
    /// author's OWN relays only, for both discovery- and content-kind atoms
    /// alike — `build_candidates` doesn't even look at the skeleton anymore.
    #[test]
    fn build_candidates_never_includes_indexer_relays() {
        let dir = FixtureDirectory::new()
            .with_write(pk('a'), [test_relay(0)])
            .with_indexer(test_relay(99));

        let content_atom = cf_kind1(Some(BTreeSet::from([pk('a')])));
        let (_, content_authors) = Skeleton::of(&content_atom);
        let content_candidates = build_candidates(&content_authors, &dir);
        assert!(!content_candidates[&pk('a')]
            .iter()
            .any(|lr| lr.lane == Lane::IndexerDiscovery));

        let discovery_atom = ConcreteFilter {
            kinds: Some(BTreeSet::from([3u16])),
            authors: Some(BTreeSet::from([pk('a')])),
            ..ConcreteFilter::default()
        };
        let (_, discovery_authors) = Skeleton::of(&discovery_atom);
        let discovery_candidates = build_candidates(&discovery_authors, &dir);
        assert!(!discovery_candidates[&pk('a')]
            .iter()
            .any(|lr| lr.lane == Lane::IndexerDiscovery));
        assert_eq!(
            discovery_candidates[&pk('a')],
            vec![LanedRelay::new(test_relay(0), Lane::Nip65Write)]
        );
    }

    /// Own-relay hints (`Hint`/`Provenance` lanes) DO count toward `k`
    /// (owner-resolved §9-decision-3); a `UserConfigured` extra does not —
    /// only those two lanes survive `build_candidates`' filter.
    #[test]
    fn build_candidates_keeps_hint_and_provenance_extras_drops_user_configured() {
        let dir = FixtureDirectory::new()
            .with_write(pk('a'), [test_relay(0)])
            .with_extra(pk('a'), Lane::Hint, [test_relay(1)])
            .with_extra(pk('a'), Lane::Provenance, [test_relay(2)])
            .with_extra(pk('a'), Lane::UserConfigured, [test_relay(3)]);

        let candidates = build_candidates(&BTreeSet::from([pk('a')]), &dir);
        let urls: BTreeSet<RelayUrl> = candidates[&pk('a')]
            .iter()
            .map(|lr| lr.url.clone())
            .collect();
        assert_eq!(
            urls,
            BTreeSet::from([test_relay(0), test_relay(1), test_relay(2)]),
            "Hint/Provenance extras count toward k; UserConfigured does not"
        );
    }

    #[test]
    fn provenance_for_outbox_yields_one_entry_per_author_relay_pair() {
        let dir = FixtureDirectory::new().with_write(pk('a'), [test_relay(0), test_relay(1)]);
        let atom = cf_kind1(Some(BTreeSet::from([pk('a')])));
        let (_, authors) = Skeleton::of(&atom);
        let candidates = build_candidates(&authors, &dir);
        let coverage = solve(&CoverageInput {
            candidates: candidates.clone(),
            k: 2,
            cap: 10,
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
