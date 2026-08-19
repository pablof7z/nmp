//! [`Router`] — the per-relay planning owner (M2 plan §2.7, §4.1).
//! [`Router::compile`] is the whole-demand invalidation path for changed
//! routing facts or reactive roots. Ordinary app opens use
//! [`Router::admit`]: compile one unsent cohort without rewriting running
//! requests. Both paths share the same routing/coalescing compiler and wire
//! token namespace.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::{ConcreteFilter, ContextualAtom, ReadRouting, RelaySessionKey, RoutingEvidence};
use nmp_store::{coverage_claim_atoms, coverage_key, CoverageKey};

use crate::budget::CompileBudget;
use crate::coalesce::{EntryOwnership, RuleRegistry};
use crate::diag::Diagnostics;
use crate::facts::{PublicKey, RelayUrl, RoutingFacts};
use crate::ownership::{
    reduce_outbox_shortfall, AdmissionWork, CompileOutcome, ExactFilterKey, FullMetadataWork,
    RefusalOwner, RequestKey, RequestMetadataUpdate, RequestOwnerContribution, RequestReplacement,
    WithdrawalWork,
};
use crate::plan::{diff_plans, BudgetShortfall, DemandKey, RelayPlan, SubId, WireReq};
use crate::route::{self, AtomClass, RouteProvenance, Skeleton};
use crate::solver::{self, CoverageInput, Shortfall};
use crate::wire_id;

/// The equal-context-only coalescing gate (Fable D, "locus fixed"): two
/// atoms only ever share wire work if their FULL context matches. Bagged
/// and coalesced entirely inside `Router::compile` -- `coalesce.rs` itself
/// stays PURE selection-only and never learns this type exists.
/// One relay's not-yet-coalesced bag entry, PER context partition: a
/// materialized (filter, single-lane provenance, coverage_claims coverage-key)
/// triple -- selection-only, exactly what `coalesce.rs::coalesce_with`
/// (unchanged, context-free) has always taken.
type BagEntry = (ConcreteFilter, Vec<RouteProvenance>, EntryOwnership);
type SessionBag = BTreeMap<RelaySessionKey, BTreeMap<ReadRouting, Vec<BagEntry>>>;

struct RelayCapOutcome {
    limited_demands: BTreeSet<DemandKey>,
    refused_sessions: BTreeSet<RelaySessionKey>,
    refused_demands: BTreeMap<RelaySessionKey, BTreeSet<DemandKey>>,
    refused_coverage_assignments: BTreeSet<(DemandKey, PublicKey)>,
}

/// Every `ReadRouting::Auto` atom sharing one `(Skeleton)`,
/// accumulated so the single `Auto` path can run all of its lanes over the
/// group at once.
#[derive(Default)]
struct AutoAtomGroup {
    /// Routing facts keyed by the author they were projected for — the
    /// coverage solve's extra candidates.
    evidence_by_author: BTreeMap<PublicKey, BTreeSet<RoutingEvidence>>,
    /// Every member atom's routing facts, unioned. Routed directly ONLY when
    /// the group is unbound: an author-bearing group's hints already reach it
    /// through `evidence_by_author` above, inside the solve. The union is
    /// exactly why the direct lane must stay narrow — one member's `nevent`
    /// hint appears here on behalf of the whole group.
    routing_evidence: BTreeSet<RoutingEvidence>,
    /// Union of the resolved authors across the group.
    authors: BTreeSet<PublicKey>,
    /// True iff some atom here left `authors` UNBOUND — asked about everyone,
    /// as opposed to bound-but-resolved-to-nobody, which asked about no one
    /// and is not this. Such an atom is only covered by the author-erased
    /// skeleton, so when it is present the additive lanes must carry that
    /// bare skeleton rather than the group's author union — the union would
    /// silently narrow an unbounded demand into one that names other
    /// people's authors. It is also the only case whose hints have nowhere
    /// to go but the direct lane.
    unbounded: bool,
    demands: BTreeSet<DemandKey>,
    authors_by_demand: BTreeMap<DemandKey, BTreeSet<PublicKey>>,
}

/// Apply the ONE whole-demand relay ceiling after every routing lane has
/// materialized. The previous implementation handed the full `cap` to each
/// outbox skeleton independently and then added indexer/app/fallback/pinned
/// relays outside those solves, so the assembled plan could exceed `cap` by
/// an arbitrary factor.
///
/// Selection is deterministic and coverage-biased: the relay carrying the
/// most typed route facts wins, with the canonical relay URL as the stable
/// tie-break. Refused relays are removed from the only bag that can become a
/// [`RelayPlan`], and every coverage_claims atom they would have served is retained
/// as explicit local-limit evidence. This is intentionally conservative: if
/// a cap removes an additive or redundant planned source, the demand still
/// reports that local limit instead of pretending the smaller plan was the
/// complete requested acquisition.
fn apply_global_relay_cap(bag: &mut SessionBag, cap: usize) -> RelayCapOutcome {
    if bag.len() <= cap {
        return RelayCapOutcome {
            limited_demands: BTreeSet::new(),
            refused_sessions: BTreeSet::new(),
            refused_demands: BTreeMap::new(),
            refused_coverage_assignments: BTreeSet::new(),
        };
    }

    let mut ranked: Vec<(RelaySessionKey, usize, usize)> = bag
        .iter()
        .map(|(session, by_source)| {
            let coverage_assignments: BTreeSet<_> = by_source
                .values()
                .flatten()
                .flat_map(|(_, _, ownership)| ownership.coverage_assignments.iter().cloned())
                .collect();
            let secondary = by_source
                .values()
                .flatten()
                .map(|(_, provenance, ownership)| {
                    provenance.len().max(ownership.coverage_claims.len()).max(1)
                })
                .sum();
            (session.clone(), coverage_assignments.len(), secondary)
        })
        .collect();
    ranked.sort_by(
        |(a_url, a_coverage, a_secondary), (b_url, b_coverage, b_secondary)| {
            b_coverage
                .cmp(a_coverage)
                .then_with(|| b_secondary.cmp(a_secondary))
                .then_with(|| a_url.cmp(b_url))
        },
    );

    let selected: BTreeSet<RelaySessionKey> = ranked
        .iter()
        .take(cap)
        .map(|(session, _, _)| session.clone())
        .collect();
    let refused: BTreeSet<RelaySessionKey> = bag
        .keys()
        .filter(|relay| !selected.contains(*relay))
        .cloned()
        .collect();

    let mut limited_demands = BTreeSet::new();
    let mut refused_demands = BTreeMap::new();
    let mut refused_coverage_assignments = BTreeSet::new();
    for session in &refused {
        if let Some(by_source) = bag.get(session) {
            for (_, _, ownership) in by_source.values().flatten() {
                limited_demands.extend(ownership.owner_demands.iter().cloned());
                refused_demands
                    .entry(session.clone())
                    .or_insert_with(BTreeSet::new)
                    .extend(ownership.owner_demands.iter().cloned());
                refused_coverage_assignments.extend(ownership.coverage_assignments.iter().cloned());
            }
        }
    }

    bag.retain(|session, _| selected.contains(session));
    RelayCapOutcome {
        limited_demands,
        refused_sessions: refused,
        refused_demands,
        refused_coverage_assignments,
    }
}

/// Cut `session_reqs` down to `allowed` subscriptions, returning the ones
/// removed so the caller can report every coverage key they carried.
///
/// Selection is deterministic, coverage-biased, and — the property that
/// matters most — STABLE across recompiles:
///
/// An INCUMBENT (an exact unchanged request the previous plan already carried)
/// outranks every newcomer. Without that, a saturated relay
/// would evict whatever the newest atom outranked, re-admit it next compile,
/// and oscillate forever: a CLOSE plus a REQ per recompile, caused by the
/// budget itself. Ranking incumbents first makes a bound budget quiet — the
/// relay keeps serving what it is already serving, and the demand that
/// cannot fit is refused explicitly rather than swapped in and out.
///
/// Below incumbency, the rank is the same coverage bias
/// [`apply_global_relay_cap`] uses (most coverage_claims atoms, then most route
/// facts), with the canonical filter hash as a stable final tie-break so the
/// outcome never depends on mint order or map iteration.
fn refuse_over_budget(
    session_reqs: &mut Vec<WireReq>,
    allowed: usize,
    prior_reqs: Option<&Vec<WireReq>>,
) -> Vec<WireReq> {
    let incumbents: BTreeSet<&SubId> = prior_reqs
        .into_iter()
        .flatten()
        .map(|req| &req.sub_id)
        .collect();
    let mut ranked: Vec<WireReq> = std::mem::take(session_reqs);
    ranked.sort_by(|a, b| {
        incumbents
            .contains(&b.sub_id)
            .cmp(&incumbents.contains(&a.sub_id))
            .then_with(|| {
                b.coverage_assignments
                    .len()
                    .cmp(&a.coverage_assignments.len())
            })
            .then_with(|| b.coverage_claims.len().cmp(&a.coverage_claims.len()))
            .then_with(|| b.provenance.len().cmp(&a.provenance.len()))
            .then_with(|| a.filter.hash().cmp(&b.filter.hash()))
    });
    let refused = ranked.split_off(allowed.min(ranked.len()));
    // A freshly compiled plan starts in canonical token order. Exact delta
    // withdrawal may later use swap-removal; `diff_plans` keys requests and
    // does not depend on retained Vec order.
    ranked.sort_by(|a, b| a.sub_id.cmp(&b.sub_id));
    *session_reqs = ranked;
    refused
}

/// Push `(filter, provenance, coverage_key(atom))` into `bag[relay][ctx]`
/// for every `(relay, provenance)` pair in `routes` — the shared
/// materialization step `compile` uses for every lane. A no-op when
/// `routes` is empty (no configured relays for that lane, or the lane's
/// gate didn't fire).
fn push_routes(
    bag: &mut SessionBag,
    filter: &ConcreteFilter,
    source: &ReadRouting,
    authenticate_as: Option<nostr::PublicKey>,
    routes: Vec<(RelayUrl, RouteProvenance)>,
    ownership: &EntryOwnership,
) {
    if routes.is_empty() {
        return;
    }
    for (relay, prov) in routes {
        bag.entry(RelaySessionKey::new(relay, authenticate_as))
            .or_default()
            .entry(source.clone())
            .or_default()
            .push((filter.clone(), vec![prov], ownership.clone()));
    }
}

fn exact_atom_ownership(atom: ContextualAtom) -> EntryOwnership {
    EntryOwnership {
        coverage_claims: coverage_claim_atoms(&atom)
            .iter()
            .map(coverage_key)
            .collect(),
        owner_demands: BTreeSet::from([DemandKey::for_atom(&atom)]),
        coverage_assignments: BTreeSet::new(),
    }
}

/// Ownership for one entry on the `Auto` path.
///
/// `authors` are the authors this entry serves; `coverage_authors` are the
/// ones it is a solved coverage assignment for. `unbounded` says the entry
/// also serves an atom that named no authors at all, which is a claim and an
/// owner the per-author walk below cannot produce: with an empty author set
/// there is nothing to iterate, and `is_disjoint` against it is vacuously
/// true, so an unbounded demand would own nothing and prove no coverage.
/// It gets the author-erased skeleton's own claim instead, which is exactly
/// the atom it reaches the wire as.
fn auto_ownership(
    skeleton: &Skeleton,
    authenticate_as: Option<nostr::PublicKey>,
    group: &AutoAtomGroup,
    authors: &BTreeSet<PublicKey>,
    coverage_authors: &BTreeSet<PublicKey>,
    unbounded: bool,
) -> EntryOwnership {
    let routing = ReadRouting::Auto;
    let evidence_by_author = &group.evidence_by_author;
    let authors_by_demand = &group.authors_by_demand;
    let mut coverage_claims: BTreeSet<_> = authors
        .iter()
        .map(|author| {
            let atom = ContextualAtom {
                filter: skeleton.with_authors(BTreeSet::from([*author])),
                routing: routing.clone(),
                authenticate_as,
                routing_evidence: evidence_by_author.get(author).cloned().unwrap_or_default(),
            };
            coverage_key(&atom)
        })
        .collect();
    let mut owner_demands: BTreeSet<_> = authors_by_demand
        .iter()
        .filter_map(|(demand, demand_authors)| {
            (!authors.is_disjoint(demand_authors)).then_some(demand.clone())
        })
        .collect();
    if unbounded {
        coverage_claims.insert(coverage_key(&ContextualAtom {
            filter: skeleton.with_authors(BTreeSet::new()),
            routing: routing.clone(),
            authenticate_as,
            routing_evidence: group.routing_evidence.clone(),
        }));
        owner_demands.extend(
            authors_by_demand
                .iter()
                .filter_map(|(demand, demand_authors)| {
                    demand_authors.is_empty().then_some(demand.clone())
                }),
        );
    }
    let coverage_assignments = authors_by_demand
        .iter()
        .flat_map(|(demand, demand_authors)| {
            coverage_authors
                .intersection(demand_authors)
                .map(|author| (demand.clone(), *author))
        })
        .collect();
    EntryOwnership {
        coverage_claims,
        owner_demands,
        coverage_assignments,
    }
}

/// Everything one compile of a demand set decides, before any of it is
/// installed into a [`Router`].
pub(crate) struct CompiledDemand {
    pub(crate) plan: RelayPlan,
    pub(crate) replacements: BTreeSet<RequestReplacement>,
    pub(crate) uncovered_by_demand: BTreeMap<DemandKey, BTreeMap<PublicKey, Shortfall>>,
    pub(crate) cap_refused_demands: BTreeMap<RelaySessionKey, BTreeSet<DemandKey>>,
    pub(crate) cap_refused_coverage_assignments: BTreeSet<(DemandKey, PublicKey)>,
    pub(crate) budget_refused_requests: Vec<(RelaySessionKey, WireReq)>,
}

/// Route, coalesce and token-assign `demand` into one relay plan.
///
/// A pure function of its arguments: it reads no ownership index and writes
/// none, so nothing it does can be seen by a running [`Router`] until that
/// router installs the result. That is what lets [`Router::admit`] compile a
/// cohort against an empty incumbent namespace by passing an empty map,
/// rather than detaching two dozen live indexes and putting them back.
///
/// `incumbent_reqs` is the request namespace this compile may match against:
/// [`Router::compile`] passes the running plan, so a byte-identical successor
/// keeps its wire token and an incumbent outranks a newcomer under a
/// saturated subscription budget. [`Router::admit`] passes an EMPTY map --
/// that is exactly what "compiled in an empty incumbent namespace" means, and
/// it is why an admitted request always arrives on a freshly minted token.
/// `next_token` is the router's monotonic mint counter, threaded in and out
/// rather than copied, so no compile -- isolated or not -- can rewind it.
pub(crate) fn compile_demand(
    rules: &RuleRegistry,
    next_token: &mut u64,
    incumbent_reqs: &BTreeMap<RelaySessionKey, Vec<WireReq>>,
    demand: &BTreeSet<ContextualAtom>,
    facts: &dyn RoutingFacts,
    budget: &CompileBudget,
) -> CompiledDemand {
    // Step 1: group demand by (Skeleton) / classify
    // explicit. Classification is by DECLARED `ReadRouting` and nothing
    // else — never by filter shape. Grouping by authenticated identity
    // alongside the skeleton keeps the seam ready for a future
    // present authenticated identity (#8's NIP-42 AUTH) without needing a
    // second widening later; every atom reaching this branch shares
    // `routing: Auto` by construction (that's the `classify` arm that
    // produced it), so it isn't tracked per-group.
    //
    // An atom whose selection resolves NO authors joins the SAME group
    // as its author-bearing siblings, because the skeleton it hashes to
    // is the same one. That is deliberate — it is one `Auto` path, and
    // the group records the fact via `unbounded` so the additive lanes
    // widen back to the bare skeleton instead of narrowing the
    // unbounded atom to the group's authors.
    let mut auto_groups: BTreeMap<(Skeleton, Option<PublicKey>), AutoAtomGroup> = BTreeMap::new();
    // Every `Auto` demand's resolved authors, flat across groups — the
    // shortfall reduction at the end of this function walks demands, not
    // groups.
    let mut auto_authors_by_demand: BTreeMap<DemandKey, BTreeSet<PublicKey>> = BTreeMap::new();
    // #107: query-declared `ReadRouting::Explicit(relays)` atoms — kept
    // in their OWN collection, since these must skip every additive lane
    // (indexer/app/fallback) below, not just the solve.
    let mut exact_atoms: Vec<(ContextualAtom, BTreeSet<RelayUrl>)> = Vec::new();
    for atom in demand {
        match route::classify(&atom.routing) {
            AtomClass::Auto => {
                let (skeleton, authors) = Skeleton::of(&atom.filter);
                let demand = DemandKey::for_atom(atom);
                auto_authors_by_demand.insert(demand.clone(), authors.clone());
                let group = auto_groups
                    .entry((skeleton, atom.authenticate_as))
                    .or_default();
                group.demands.insert(demand.clone());
                group
                    .authors_by_demand
                    .insert(demand.clone(), authors.clone());
                group
                    .routing_evidence
                    .extend(atom.routing_evidence.iter().cloned());
                // UNBOUND, not "resolved to nobody". `Skeleton::of`
                // reports an empty author set for both `authors: None`
                // and `authors: Some(∅)`, and those are different
                // demands: the first asked about everyone, the second
                // asked about nobody. `coverage_claims` already draws
                // exactly that line, and the two halves of this change
                // must not disagree about it.
                if atom.filter.authors.is_none() {
                    group.unbounded = true;
                }
                for author in authors {
                    group.authors.insert(author);
                    group
                        .evidence_by_author
                        .entry(author)
                        .or_default()
                        .extend(atom.routing_evidence.iter().cloned());
                }
            }
            AtomClass::Exact(relays) => {
                exact_atoms.push((atom.clone(), relays));
            }
        }
    }

    // Step 2 + 3: route (coverage-solve outbox groups / pinned lookup),
    // apply the additive indexer/app/fallback lanes OUTSIDE the solve
    // (Unit B, `routing-and-ownership.md` §2.1/§2.2 — never counted
    // toward `k`), and materialize each relay's bag of (filter,
    // context, single-lane provenance, coverage_claims) entries. `coverage_claims` is
    // the coverage-attribution ruling's per-atom `CoverageKey` (§2):
    // each entry here is exactly one pre-coalesce demand atom (one
    // author, for outbox; the full/shortfall author set, for an
    // additive lane; the pinned atom itself, for pinned), so it
    // contributes exactly one key, later unioned by `coalesce_with`
    // alongside provenance as same-skeleton, SAME-CONTEXT atoms merge
    // (Fable D: equal-context-only).
    let mut bag: SessionBag = BTreeMap::new();
    let mut uncovered_by_demand: BTreeMap<DemandKey, BTreeMap<PublicKey, Shortfall>> =
        BTreeMap::new();

    for ((skeleton, authenticate_as), group) in &auto_groups {
        let authenticate_as = *authenticate_as;
        let source = ReadRouting::Auto;
        let evidence_by_author = &group.evidence_by_author;
        let authors = &group.authors;
        let authors_by_group_demand = &group.authors_by_demand;
        // The filter the additive lanes carry. The group's author union
        // normally, but the author-erased skeleton whenever some atom
        // here named no authors: only the bare skeleton covers such an
        // atom, and it also supersets every author-bearing sibling, so
        // widening is both necessary and sufficient.
        let lane_filter = if group.unbounded {
            skeleton.with_authors(BTreeSet::new())
        } else {
            skeleton.with_authors(authors.clone())
        };
        let candidates = route::build_candidates(authors, facts);
        let mut candidates = candidates;
        route::add_projected_candidates(&mut candidates, evidence_by_author);
        let coverage = solver::solve(&CoverageInput {
            candidates: candidates.clone(),
            k: 2,
            // Per-skeleton limiting is the defect #20 removes. Build
            // each skeleton's honest k-cover first; the ONE assembled-
            // plan ceiling below accounts for every skeleton and every
            // additive/pinned lane together.
            //
            // #505 asked whether threading the real (or a "generous
            // multiple" of the) whole-demand `cap` in here instead of
            // `usize::MAX` would bound `solver::solve`'s greedy loop
            // without changing the assembled plan. It would not, and is
            // deliberately NOT done:
            //   1. `solve`'s iteration count is already bounded by
            //      `sum(per-author ceilings) <= k * authors_in_group`
            //      (`k` is 2 here) regardless of `cap` -- every
            //      iteration's selected relay must satisfy at least one
            //      outstanding (author, slot) need, or the loop exits
            //      via the "no candidate relay helps" branch. So any
            //      `cap` at or above `2 * authors_in_group` is a no-op
            //      (no perf change), and the O(authors^2) cost the
            //      issue flags is the per-iteration O(authors *
            //      candidates) rescan, not iteration count.
            //   2. Any `cap` BELOW that natural bound stops the solve
            //      before every author reaches `k`, for exactly the
            //      relay-diverse (low-overlap) follow sets that make
            //      this slow in the first place -- reintroducing the
            //      truncation defect #20 removed, since a later skeleton
            //      or additive lane might have had global-cap headroom
            //      this skeleton never got to use, changing both the
            //      shortfall diagnostics and the wire plan.
            // A real fix would make the per-iteration scores rescan
            // incremental instead of touching `cap`; out of scope here.
            cap: usize::MAX,
        });
        for demand in &group.demands {
            let exact: BTreeMap<_, _> = authors_by_group_demand[demand]
                .iter()
                .filter_map(|author| {
                    coverage
                        .shortfall
                        .get(author)
                        .cloned()
                        .map(|fact| (*author, fact))
                })
                .collect();
            if !exact.is_empty() {
                uncovered_by_demand.insert(demand.clone(), exact);
            }
        }

        // ONE bag entry per (relay, skeleton), carrying every author this
        // relay was solved for -- not one entry per (author, relay).
        //
        // `provenance_for_outbox` deliberately yields one route per
        // (author, relay) so each route keeps its own provenance, and this
        // loop used to turn each of those into its own single-author
        // filter. For an UNLIMITED demand `coalesce_with` re-joined them
        // downstream and nothing showed. For a LIMITED one it could not --
        // `neither_limited` refuses any filter carrying a `limit` -- so a
        // bounded feed reached the wire as one REQ per author: ~351 wanted
        // subscriptions over a 1055-author follow list, against relay caps
        // of ~20 (#937).
        //
        // Re-joining here rather than relaxing `neither_limited` is the
        // point. Those atoms were never independent demands competing for
        // rows; they are ONE demand that routing fanned for provenance,
        // and a window belongs to the feed rather than to each author in
        // it (owner ruling, #937). Two genuinely independent limited
        // watches still meet `neither_limited` downstream and still stay
        // apart -- that rule is untouched.
        //
        // Coverage needs no special handling: a limited filter poisons its
        // whole EOSE attribution (`attribution.rs`, `limited:
        // filter.limit.is_some()`), so a bounded fetch records no coverage
        // merged or unmerged. The per-author keys are still coverage_claims so an
        // UNLIMITED feed goes on proving coverage for each author it named.
        let mut by_relay: BTreeMap<RelayUrl, (BTreeSet<PublicKey>, Vec<RouteProvenance>)> =
            BTreeMap::new();
        for (relay, prov) in route::provenance_for_coverage(&coverage, &candidates) {
            let entry = by_relay.entry(relay).or_default();
            entry.0.extend(prov.covers_authors.iter().cloned());
            entry.1.push(prov);
        }
        for (relay, (relay_authors, provenance)) in by_relay {
            let filter = skeleton.with_authors(relay_authors.clone());
            // The narrow per-author keys, one per author this relay
            // serves. `coverage_key` window-erases and zeroes
            // `routing_evidence`, so each key is exactly the key the
            // resolver's own per-author atom hashes to -- which is what
            // lets one merged REQ absorb them all.
            let ownership = auto_ownership(
                skeleton,
                authenticate_as,
                group,
                &relay_authors,
                &relay_authors,
                false,
            );
            bag.entry(RelaySessionKey::new(relay, authenticate_as))
                .or_default()
                .entry(source.clone())
                .or_default()
                .push((filter, provenance, ownership));
        }

        let lane_ownership = auto_ownership(
            skeleton,
            authenticate_as,
            group,
            authors,
            &BTreeSet::new(),
            group.unbounded,
        );

        // The group's own routing facts, routed DIRECTLY -- but ONLY for
        // an unbound group.
        //
        // Hints reach an author-bearing group already, as per-author
        // candidates in the solve above (`add_projected_candidates`),
        // where they compete for the k=2 slots and earn coverage like
        // any other relay. An unbound group has no authors to key those
        // candidates on, so its hints have nowhere to enter and would
        // simply vanish. This lane is that gap and nothing more.
        //
        // Running it unconditionally would ADD a lane author-bearing
        // atoms never had: a hinted relay would get a REQ outside the
        // solve and outside coverage, `routing_evidence` is unioned
        // across the group so one member's `nevent` hint would drag
        // every sibling's filter to that relay, and the durable claim
        // would cover every author in the group. That is a behaviour
        // expansion, not a consequence of collapsing two routing values,
        // so it stays out.
        if group.unbounded {
            push_routes(
                &mut bag,
                &lane_filter,
                &source,
                authenticate_as,
                route::provenance_for_projected(&group.routing_evidence),
                &lane_ownership,
            );
        }

        // Operator app policy supplements the group's full author set,
        // and routes every atom including the unbounded ones (closes #7
        // — the authorless-routing-lane gap).
        let additive = route::operator_app_routes(facts, authors);
        push_routes(
            &mut bag,
            &lane_filter,
            &source,
            authenticate_as,
            additive,
            &lane_ownership,
        );

        // Additive fallback lane: routes exactly the shortfall authors,
        // iff no appRelay is configured. `Coverage.shortfall` above has
        // already recorded the shortfall regardless of whether this
        // lane fires — fallback is a lane, not coverage.
        let shortfall_authors: BTreeSet<PublicKey> = coverage.shortfall.keys().cloned().collect();
        let fallback = route::operator_fallback_routes(facts, &shortfall_authors);
        let fallback_ownership = auto_ownership(
            skeleton,
            authenticate_as,
            group,
            &shortfall_authors,
            &BTreeSet::new(),
            false,
        );
        push_routes(
            &mut bag,
            &skeleton.with_authors(shortfall_authors),
            &source,
            authenticate_as,
            fallback,
            &fallback_ownership,
        );
    }

    // #107: an explicit, query-declared relay set — route DIRECTLY to
    // it. NO additive lane (indexer/app/fallback) is ever applied here:
    // that's the #107 Contract's core guarantee ("Explicit author
    // filters never contact directory, author-outbox, app, fallback, or
    // indexer relays").
    for (atom, relays) in &exact_atoms {
        let filter = &atom.filter;
        let authenticate_as = atom.authenticate_as;
        let source = ReadRouting::Explicit(relays.iter().cloned().collect());
        let ownership = exact_atom_ownership(atom.clone());
        for (relay, prov) in route::provenance_for_exact(relays) {
            bag.entry(RelaySessionKey::new(relay, authenticate_as))
                .or_default()
                .entry(source.clone())
                .or_default()
                .push((filter.clone(), vec![prov], ownership.clone()));
        }
    }

    // Step 4: enforce the ONE whole-demand ceiling over the fully
    // materialized bag. Nothing removed here can reach coalescing, the
    // plan, or the wire; its contextual coverage keys remain as exact
    // local-limit evidence.
    let RelayCapOutcome {
        mut limited_demands,
        mut refused_sessions,
        refused_demands: cap_refused_demands,
        refused_coverage_assignments: cap_refused_coverage_assignments,
    } = apply_global_relay_cap(&mut bag, budget.relay_cap());

    // Step 5 + 6: per relay, PER CONTEXT PARTITION, dedup + widen-only
    // coalesce (`coalesce.rs` stays pure selection-only, Fable D "locus
    // fixed" -- partitioning by `ContextKey` here is what makes
    // coalescing equal-context-only, never a change to the rule
    // engine itself), then ALLOCATE each survivor's wire token by
    // matching it against the previous plan's filters for the SAME
    // partition (`wire_id::assign`, #899).
    //
    // Wire ids used to be DERIVED from the filter's author-erased
    // `Skeleton`. That made author churn free (same skeleton, same id,
    // one overwriting REQ) but it was not injective: two filters the
    // coalescer REFUSED to merge -- `neither_limited` poisons every rule
    // the moment either side carries a `limit`, and `dedup_only()` holds
    // no author union at all -- minted the SAME id, and `diff_plans`
    // (keyed by `SubId`) then silently dropped one of them, forever.
    // Allocation buys back injectivity. The previous plan still tells a
    // byte-changing successor which request it replaces, but the
    // successor receives a fresh token and EngineCore offers it before
    // retiring the predecessor after the exact commit edge.
    let mut mint_counter = *next_token;
    let mut reqs: BTreeMap<RelaySessionKey, Vec<WireReq>> = BTreeMap::new();
    let mut replacements: BTreeSet<RequestReplacement> = BTreeSet::new();
    let mut subscription_shortfalls: BTreeMap<RelaySessionKey, BudgetShortfall> = BTreeMap::new();
    let mut budget_refused_requests: Vec<(RelaySessionKey, WireReq)> = Vec::new();
    for (session, by_source) in bag {
        let relay = session.relay.clone();
        let authenticate_as = session.authenticate_as;
        let mut session_reqs: Vec<WireReq> = Vec::new();
        for (source, entries) in by_source {
            let merged = rules.coalesce_with(entries);
            let filters: Vec<ConcreteFilter> =
                merged.iter().map(|(filter, _, _)| filter.clone()).collect();

            // The matching partition: this exact relay session AND this
            // exact declared authority. Reading it back out of
            // `prev_plan` is what makes the matching state pruned by
            // construction -- there is no separate table to age out.
            let priors: Vec<(ConcreteFilter, SubId)> = incumbent_reqs
                .get(&session)
                .into_iter()
                .flatten()
                .filter(|req| req.routing == source)
                .map(|req| (req.filter.clone(), req.sub_id.clone()))
                .collect();

            let assigned = wire_id::assign(&priors, &filters, || {
                let sub_id = SubId::allocate(relay.clone(), &source, authenticate_as, mint_counter);
                mint_counter += 1;
                sub_id
            });

            session_reqs.extend(merged.into_iter().zip(assigned).map(
                |((filter, provenance, ownership), assignment)| {
                    if let Some(prior_sub_id) = assignment.predecessor {
                        replacements.insert(RequestReplacement {
                            session: session.clone(),
                            prior_sub_id,
                            next_sub_id: assignment.sub_id.clone(),
                        });
                    }
                    let provenance = provenance.into_iter().collect::<BTreeSet<_>>();
                    WireReq {
                        sub_id: assignment.sub_id,
                        filter,
                        routing: source.clone(),
                        provenance,
                        coverage_claims: ownership.coverage_claims,
                        owner_demands: ownership.owner_demands,
                        coverage_assignments: ownership.coverage_assignments,
                    }
                },
            ));
        }
        session_reqs.sort_by(|a, b| a.sub_id.cmp(&b.sub_id));

        // Step 6b: the PER-RELAY SUBSCRIPTION BUDGET (#931). Enforced
        // here, after coalescing and after token assignment, because
        // both of those decide what the count actually IS: the collapse
        // is what turns a 300-value catalog into one subscription, and
        // the assignment is what tells an INCUMBENT subscription (one
        // the previous plan already carried) apart from a newcomer.
        //
        // A relay that advertised nothing is unbudgeted -- see
        // `crate::budget` for why absence is not a number.
        let planned = session_reqs.len();
        match budget.max_subscriptions(&relay) {
            None => {}
            Some(allowed) if planned <= allowed => {}
            Some(allowed) => {
                let refused =
                    refuse_over_budget(&mut session_reqs, allowed, incumbent_reqs.get(&session));
                for req in &refused {
                    limited_demands.extend(req.owner_demands.iter().cloned());
                }
                budget_refused_requests.extend(
                    refused
                        .iter()
                        .cloned()
                        .map(|request| (session.clone(), request)),
                );
                subscription_shortfalls.insert(
                    session.clone(),
                    BudgetShortfall {
                        budget: allowed,
                        planned,
                        refused: refused.len(),
                    },
                );
                // A relay advertising ZERO concurrent subscriptions
                // cannot be planned at all. That is a whole-session
                // refusal, so it takes the same seam the whole-demand
                // ceiling uses and stays absent from `reqs` -- the
                // invariant every `refused_sessions` reader relies on.
                if session_reqs.is_empty() {
                    refused_sessions.insert(session);
                    continue;
                }
            }
        }

        reqs.insert(session, session_reqs);
    }
    *next_token = mint_counter;

    let selected_assignments: BTreeMap<_, BTreeSet<_>> = reqs
        .iter()
        .flat_map(|(session, requests)| {
            requests.iter().flat_map(move |request| {
                request
                    .coverage_assignments
                    .iter()
                    .map(move |assignment| (assignment.clone(), session.clone()))
            })
        })
        .fold(BTreeMap::new(), |mut indexed, (assignment, session)| {
            indexed
                .entry(assignment)
                .or_insert_with(BTreeSet::new)
                .insert(session);
            indexed
        });
    let refused_coverage_assignments: BTreeSet<_> = cap_refused_coverage_assignments
        .iter()
        .cloned()
        .chain(
            budget_refused_requests
                .iter()
                .flat_map(|(_, request)| request.coverage_assignments.iter().cloned()),
        )
        .collect();
    for (demand, authors) in &auto_authors_by_demand {
        let intrinsic = uncovered_by_demand.remove(demand).unwrap_or_default();
        let exact: BTreeMap<_, _> = authors
            .iter()
            .filter_map(|author| {
                let assignment = (demand.clone(), *author);
                let achieved = selected_assignments
                    .get(&assignment)
                    .map_or(0, BTreeSet::len);
                reduce_outbox_shortfall(
                    intrinsic.get(author).cloned(),
                    achieved,
                    refused_coverage_assignments.contains(&assignment),
                )
                .map(|fact| (*author, fact))
            })
            .collect();
        if !exact.is_empty() {
            uncovered_by_demand.insert(demand.clone(), exact);
        }
    }

    // The invariant the whole change exists to establish, checked in
    // RELEASE builds too (a `debug_assert!` compiles out of exactly the
    // builds that ship). Under allocation it can never fire -- each prior
    // token is assigned to at most one filter and every mint is unique --
    // which is precisely what makes it cheap enough to leave in. If it
    // ever does fire, `diff_plans` would have dropped a `WireReq` that
    // never reached the wire and never would have.
    {
        let mut seen: BTreeSet<&SubId> = BTreeSet::new();
        for req in reqs.values().flatten() {
            assert!(
                seen.insert(&req.sub_id),
                "wire sub-id injectivity violated: two WireReqs share {:?} -- \
                 diff_plans keys by SubId, so one of them could never reach the wire",
                req.sub_id
            );
        }
    }
    CompiledDemand {
        plan: RelayPlan {
            reqs,
            limited_demands,
            refused_sessions,
            subscription_shortfalls,
        },
        replacements,
        uncovered_by_demand,
        cap_refused_demands,
        cap_refused_coverage_assignments,
        budget_refused_requests,
    }
}

pub struct Router {
    pub(crate) rules: RuleRegistry,
    pub(crate) prev_plan: RelayPlan,
    pub(crate) last_diag: Diagnostics,
    /// Monotonic wire-token mint counter (#899). Never reset and never
    /// rewound, so no token is recycled within this `Router`'s lifetime.
    /// Deliberately NOT seeded randomly: the whole crate is a pure function of
    /// its inputs and the repo pins that reproducibility, while token
    /// uniqueness only ever has to hold WITHIN one router's wire namespace.
    /// Threaded through `compile_demand` by `&mut`, so a cohort compile
    /// shares the counter without sharing anything else.
    pub(crate) next_token: u64,
    /// Hoisted chain root for [`SubId::allocate`], so minting costs a handful
    /// of `fold_byte` calls rather than a JSON encode plus BLAKE3 each time.
    /// Carries no filter meaning -- see `SubId::allocate`.
    pub(crate) active_demands: BTreeMap<DemandKey, ContextualAtom>,
    pub(crate) requests_by_demand: BTreeMap<DemandKey, BTreeSet<RequestKey>>,
    pub(crate) active_by_request: BTreeMap<RequestKey, usize>,
    pub(crate) request_coverage_by_key: BTreeMap<RequestKey, BTreeSet<CoverageKey>>,
    pub(crate) request_position_by_key: BTreeMap<RequestKey, usize>,
    pub(crate) request_by_exact_filter: BTreeMap<ExactFilterKey, RequestKey>,
    /// Exact durable claim shapes physically covered when a request entered
    /// the immutable wire plan. Local owner metadata may shrink independently;
    /// these edges remain until the physical request closes.
    pub(crate) physical_claims_by_request: BTreeMap<RequestKey, BTreeSet<CoverageKey>>,
    pub(crate) requests_by_physical_claim: BTreeMap<CoverageKey, BTreeSet<RequestKey>>,
    pub(crate) physical_contributions_by_request:
        BTreeMap<RequestKey, BTreeMap<DemandKey, RequestOwnerContribution>>,
    pub(crate) requests_by_physical_demand: BTreeMap<DemandKey, BTreeSet<RequestKey>>,
    pub(crate) request_owner_contributions:
        BTreeMap<RequestKey, BTreeMap<DemandKey, RequestOwnerContribution>>,
    pub(crate) request_claim_owner_counts: BTreeMap<(RequestKey, CoverageKey), usize>,
    pub(crate) request_provenance_owner_counts: BTreeMap<(RequestKey, RouteProvenance), usize>,
    pub(crate) request_demand_coverage_owner_counts: BTreeMap<(RequestKey, CoverageKey), usize>,
    pub(crate) coverage_assignment_requests: BTreeMap<(DemandKey, PublicKey), BTreeSet<RequestKey>>,
    pub(crate) refused_coverage_assignments_by_demand: BTreeMap<DemandKey, BTreeSet<PublicKey>>,
    pub(crate) active_outbox_authors: BTreeMap<PublicKey, usize>,
    pub(crate) diagnostic_author_refs: BTreeMap<RelaySessionKey, BTreeMap<PublicKey, usize>>,
    pub(crate) uncovered_by_demand: BTreeMap<DemandKey, BTreeMap<PublicKey, Shortfall>>,
    pub(crate) uncovered_owners_by_author: BTreeMap<PublicKey, BTreeMap<DemandKey, Shortfall>>,
    pub(crate) refusals_by_demand: BTreeMap<DemandKey, BTreeMap<RelaySessionKey, RefusalOwner>>,
    pub(crate) refused_request_owner_counts: BTreeMap<RequestKey, usize>,
    pub(crate) refused_owner_counts_by_session: BTreeMap<RelaySessionKey, usize>,
    pub(crate) admission_work: AdmissionWork,
    pub(crate) full_metadata_work: FullMetadataWork,
    pub(crate) withdrawal_work: WithdrawalWork,
}

impl Router {
    pub fn new(rules: RuleRegistry) -> Self {
        Self {
            rules,
            prev_plan: RelayPlan::default(),
            last_diag: Diagnostics::default(),
            next_token: 0,
            active_demands: BTreeMap::new(),
            requests_by_demand: BTreeMap::new(),
            active_by_request: BTreeMap::new(),
            request_coverage_by_key: BTreeMap::new(),
            request_position_by_key: BTreeMap::new(),
            request_by_exact_filter: BTreeMap::new(),
            physical_claims_by_request: BTreeMap::new(),
            requests_by_physical_claim: BTreeMap::new(),
            physical_contributions_by_request: BTreeMap::new(),
            requests_by_physical_demand: BTreeMap::new(),
            request_owner_contributions: BTreeMap::new(),
            request_claim_owner_counts: BTreeMap::new(),
            request_provenance_owner_counts: BTreeMap::new(),
            request_demand_coverage_owner_counts: BTreeMap::new(),
            coverage_assignment_requests: BTreeMap::new(),
            refused_coverage_assignments_by_demand: BTreeMap::new(),
            active_outbox_authors: BTreeMap::new(),
            diagnostic_author_refs: BTreeMap::new(),
            uncovered_by_demand: BTreeMap::new(),
            uncovered_owners_by_author: BTreeMap::new(),
            refusals_by_demand: BTreeMap::new(),
            refused_request_owner_counts: BTreeMap::new(),
            refused_owner_counts_by_session: BTreeMap::new(),
            admission_work: AdmissionWork::default(),
            full_metadata_work: FullMetadataWork::default(),
            withdrawal_work: WithdrawalWork::default(),
        }
    }

    /// Recompile the whole per-relay plan from `demand`, diff vs the previous
    /// plan, store the new plan + diagnostics, and return the surgical wire
    /// delta. Use this when existing demand may genuinely need rerouting;
    /// ordinary pending app admission uses [`Self::admit`].
    ///
    /// `budget` carries every bound this compile plans within
    /// ([`CompileBudget`]): the operator's whole-demand relay ceiling, and
    /// whatever each relay advertised about itself in NIP-11. A bare
    /// `usize` still means exactly what it always meant — that relay
    /// ceiling, with no relay having advertised anything.
    pub fn compile(
        &mut self,
        demand: &BTreeSet<ContextualAtom>,
        facts: &dyn RoutingFacts,
        budget: impl Into<CompileBudget>,
    ) -> CompileOutcome {
        let budget = budget.into();
        let CompiledDemand {
            plan: mut next_plan,
            replacements,
            uncovered_by_demand,
            cap_refused_demands,
            cap_refused_coverage_assignments,
            budget_refused_requests,
        } = compile_demand(
            &self.rules,
            &mut self.next_token,
            &self.prev_plan.reqs,
            demand,
            facts,
            &budget,
        );
        // Everything below installs the compiled plan; `compile_demand`
        // itself touched none of it. Cohort admission never reaches here at
        // all -- it takes the compiled plan and appends it -- so these
        // counters now only ever count a whole-demand recompile.
        //
        // Physical diffing depends only on session, SubId, and filter. Do it
        // before moving incumbent metadata into the next immutable request.
        // `diff_plans` walks every incumbent request in `self.prev_plan`;
        // count exactly what it is about to walk.
        //
        // The incumbent `limited_demands` set is counted here too, not at
        // its point of replacement below: the `mem::take(&mut self.prev_plan)`
        // further down (for exact-position matching) empties it before that
        // point, which would make a counter placed there always read 0 -- a
        // vacuous placement of exactly the kind #1781 was about.
        // `next_plan.limited_demands` supersedes the incumbent set wholesale
        // (built only from this call's `demand`, never merged into it), so
        // what is counted here is genuinely what gets replaced.
        self.admission_work.incumbent_plan_requests_visited = self
            .admission_work
            .incumbent_plan_requests_visited
            .saturating_add(self.prev_plan.reqs.values().map(Vec::len).sum::<usize>() as u64);
        self.admission_work.incumbent_limited_entries_visited = self
            .admission_work
            .incumbent_limited_entries_visited
            .saturating_add(self.prev_plan.limited_demands.len() as u64);
        let delta = diff_plans(&self.prev_plan, &next_plan);
        self.reconcile_active_demands(
            demand
                .iter()
                .cloned()
                .map(|atom| (DemandKey::for_atom(&atom), atom))
                .collect(),
        );

        // A byte-identical physical request is not rewritten merely because
        // local owner or claim metadata changed. Preserve its already-sent
        // metadata monotonically and report only new execution/claim owners
        // to Core, including when the wire delta is empty. Consume the prior
        // plan so large metadata sets move instead of being deep-cloned.
        let mut previous_plan = std::mem::take(&mut self.prev_plan);
        let mut exact_positions: BTreeMap<_, Vec<(usize, usize)>> = BTreeMap::new();
        for (session, requests) in &next_plan.reqs {
            for (next_position, request) in requests.iter().enumerate() {
                let request_key = (session.clone(), request.sub_id.clone());
                let Some(previous_position) = self.request_position_by_key.get(&request_key) else {
                    continue;
                };
                self.full_metadata_work.requests_probed =
                    self.full_metadata_work.requests_probed.saturating_add(1);
                exact_positions
                    .entry(session.clone())
                    .or_default()
                    .push((*previous_position, next_position));
            }
        }
        let mut request_metadata_updates = Vec::new();
        let mut unchanged_request_keys = BTreeSet::new();
        let mut retired_requests = Vec::new();
        for (session, mut positions) in exact_positions {
            positions.sort_by_key(|position| Reverse(position.0));
            let previous_requests = previous_plan
                .reqs
                .get_mut(&session)
                .expect("the exact position index names an incumbent session");
            let next_requests = next_plan
                .reqs
                .get_mut(&session)
                .expect("positions were collected from the next plan");
            for (previous_position, next_position) in positions {
                let previous = previous_requests.swap_remove(previous_position);
                let request = &mut next_requests[next_position];
                if previous.routing != request.routing || previous.filter != request.filter {
                    retired_requests.push((session.clone(), previous));
                    continue;
                }
                let candidate = std::mem::replace(request, previous);
                let request_key = (session.clone(), request.sub_id.clone());
                unchanged_request_keys.insert(request_key.clone());
                let candidate_contributions: Vec<_> = candidate
                    .owner_demands
                    .iter()
                    .filter_map(|demand| {
                        self.active_demands.get(demand).map(|atom| {
                            (
                                demand.clone(),
                                Self::derive_request_owner_contribution(atom, &candidate),
                            )
                        })
                    })
                    .collect();
                self.full_metadata_work.candidate_entries_examined = self
                    .full_metadata_work
                    .candidate_entries_examined
                    .saturating_add(
                        (candidate.coverage_claims.len()
                            + candidate.owner_demands.len()
                            + candidate.coverage_assignments.len()
                            + candidate.provenance.len()) as u64,
                    );
                let mut added_coverage_claims = BTreeSet::new();
                for claim in candidate.coverage_claims {
                    if request.coverage_claims.insert(claim.clone()) {
                        added_coverage_claims.insert(claim.clone());
                    }
                }
                let mut added_owner_demands = BTreeSet::new();
                for demand in candidate.owner_demands {
                    if request.owner_demands.insert(demand.clone()) {
                        added_owner_demands.insert(demand.clone());
                    }
                }
                let mut added_assignments = BTreeSet::new();
                for assignment in candidate.coverage_assignments {
                    if request.coverage_assignments.insert(assignment.clone()) {
                        added_assignments.insert(assignment.clone());
                    }
                }
                let mut added_provenance = BTreeSet::new();
                for provenance in candidate.provenance {
                    if request.provenance.insert(provenance.clone()) {
                        added_provenance.insert(provenance);
                    }
                }
                self.add_full_request_metadata_indexes(
                    &request_key,
                    &added_owner_demands,
                    &added_assignments,
                    &added_provenance,
                );
                for (demand, contribution) in candidate_contributions {
                    self.add_request_owner_contribution(&request_key, demand, contribution);
                }
                if !added_coverage_claims.is_empty() || !added_owner_demands.is_empty() {
                    request_metadata_updates.push(RequestMetadataUpdate {
                        session: session.clone(),
                        sub_id: request.sub_id.clone(),
                        filter_hash: request.filter.hash(),
                        added_coverage_claims,
                        added_owner_demands,
                    });
                }
            }
        }

        retired_requests.extend(
            previous_plan
                .reqs
                .into_iter()
                .flat_map(|(session, requests)| {
                    requests
                        .into_iter()
                        .map(move |request| (session.clone(), request))
                }),
        );
        for (session, request) in retired_requests {
            self.remove_full_request_indexes(&session, &request);
        }
        for (session, requests) in &next_plan.reqs {
            for request in requests {
                let request_key = (session.clone(), request.sub_id.clone());
                if !unchanged_request_keys.contains(&request_key) {
                    self.add_full_request_indexes(session, request);
                }
            }
        }

        self.prev_plan = next_plan;
        self.rebuild_request_positions();
        self.project_full_diagnostics(&budget, self.rules.dropped_rules().to_vec());
        self.install_uncovered_ownership(uncovered_by_demand);
        self.rebuild_refusal_indexes(
            cap_refused_demands,
            cap_refused_coverage_assignments,
            budget_refused_requests,
        );
        CompileOutcome {
            wire: delta,
            request_metadata_updates,
            replacements,
        }
    }

    pub fn diagnostics(&self) -> &Diagnostics {
        &self.last_diag
    }

    pub fn plan(&self) -> &RelayPlan {
        &self.prev_plan
    }
}
