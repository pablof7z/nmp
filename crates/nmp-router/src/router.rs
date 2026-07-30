//! [`Router`] — the entry point (M2 plan §2.7, §4.1). Full-recompile-then-
//! diff, not delta-threading: `compile` recomputes the whole per-relay plan
//! from the engine's CURRENT demand set each call, diffs it against the
//! previous plan, stores the new plan + diagnostics, and returns the
//! surgical wire delta. This also discharges M1 nit #2 by construction: a
//! withdrawn atom simply vanishes from `demand`, so the next `compile`
//! emits its `Close` (see `dropped_handle_close_reaches_wire`, test 15).

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::{
    AccessContext, ConcreteFilter, ContextualAtom, RelaySessionKey, RoutingEvidence,
    SourceAuthority,
};
use nmp_store::{coverage_key, CoverageKey};

use crate::budget::CompileBudget;
use crate::coalesce::RuleRegistry;
use crate::diag::{self, Diagnostics};
use crate::facts::{PublicKey, RelayUrl, RoutingFacts};
use crate::plan::{diff_plans, BudgetShortfall, RelayPlan, SubId, WireDelta, WireReq};
use crate::route::{self, AtomClass, RouteKind, RouteProvenance, Skeleton};
use crate::solver::{self, CoverageInput, Shortfall, ShortfallReason};
use crate::wire_id;

/// The equal-context-only coalescing gate (Fable D, "locus fixed"): two
/// atoms only ever share wire work if their FULL context matches. Bagged
/// and coalesced entirely inside `Router::compile` -- `coalesce.rs` itself
/// stays PURE selection-only and never learns this type exists.
/// One relay's not-yet-coalesced bag entry, PER context partition: a
/// materialized (filter, single-lane provenance, absorbed coverage-key)
/// triple -- selection-only, exactly what `coalesce.rs::coalesce_with`
/// (unchanged, context-free) has always taken.
type BagEntry = (ConcreteFilter, Vec<RouteProvenance>, BTreeSet<CoverageKey>);
type SessionBag = BTreeMap<RelaySessionKey, BTreeMap<SourceAuthority, Vec<BagEntry>>>;

/// Apply the ONE whole-demand relay ceiling after every routing lane has
/// materialized. The previous implementation handed the full `cap` to each
/// outbox skeleton independently and then added indexer/app/fallback/pinned
/// relays outside those solves, so the assembled plan could exceed `cap` by
/// an arbitrary factor.
///
/// Selection is deterministic and coverage-biased: the relay carrying the
/// most typed route facts wins, with the canonical relay URL as the stable
/// tie-break. Refused relays are removed from the only bag that can become a
/// [`RelayPlan`], and every absorbed atom they would have served is retained
/// as explicit local-limit evidence. This is intentionally conservative: if
/// a cap removes an additive or redundant planned source, the demand still
/// reports that local limit instead of pretending the smaller plan was the
/// complete requested acquisition.
fn apply_global_relay_cap(
    bag: &mut SessionBag,
    cap: usize,
    uncovered_authors: &mut BTreeMap<PublicKey, Shortfall>,
) -> (BTreeSet<CoverageKey>, BTreeSet<RelaySessionKey>) {
    if bag.len() <= cap {
        return (BTreeSet::new(), BTreeSet::new());
    }

    let mut ranked: Vec<(RelaySessionKey, usize)> = bag
        .iter()
        .map(|(session, by_source)| {
            let route_facts = by_source
                .values()
                .flatten()
                .map(|(_, provenance, absorbed)| provenance.len().max(absorbed.len()).max(1))
                .sum();
            (session.clone(), route_facts)
        })
        .collect();
    ranked.sort_by(|(a_url, a_score), (b_url, b_score)| {
        b_score.cmp(a_score).then_with(|| a_url.cmp(b_url))
    });

    let selected: BTreeSet<RelaySessionKey> = ranked
        .iter()
        .take(cap)
        .map(|(session, _)| session.clone())
        .collect();
    let refused: BTreeSet<RelaySessionKey> = bag
        .keys()
        .filter(|relay| !selected.contains(*relay))
        .cloned()
        .collect();

    let mut limited = BTreeSet::new();
    let mut cap_limited_authors = BTreeSet::new();
    for session in &refused {
        if let Some(by_source) = bag.get(session) {
            for (_, provenance, absorbed) in by_source.values().flatten() {
                limited.extend(absorbed.iter().copied());
                for route in provenance {
                    if route.route_kind == RouteKind::Coverage {
                        cap_limited_authors.extend(route.covers_authors.iter().cloned());
                    }
                }
            }
        }
    }

    // Preserve the router diagnostic's historical per-author floor while
    // moving cap enforcement to the assembled plan. Intrinsic no-candidate /
    // fewer-than-k evidence from the uncapped solve remains more specific
    // and is never overwritten by this cap-derived fact.
    for author in cap_limited_authors {
        if uncovered_authors.contains_key(&author) {
            continue;
        }
        let achieved: BTreeSet<RelayUrl> = selected
            .iter()
            .filter(|session| {
                bag.get(*session).is_some_and(|by_source| {
                    by_source.values().flatten().any(|(_, provenance, _)| {
                        provenance.iter().any(|route| {
                            route.route_kind == RouteKind::Coverage
                                && route.covers_authors.contains(&author)
                        })
                    })
                })
            })
            .map(|session| session.relay.clone())
            .collect();
        uncovered_authors.insert(
            author,
            Shortfall {
                requested_k: 2,
                achieved: achieved.len(),
                reason: ShortfallReason::CapExhausted,
            },
        );
    }

    bag.retain(|session, _| selected.contains(session));
    (limited, refused)
}

/// Cut `session_reqs` down to `allowed` subscriptions, returning the ones
/// removed so the caller can report every coverage key they carried.
///
/// Selection is deterministic, coverage-biased, and — the property that
/// matters most — STABLE across recompiles:
///
/// An INCUMBENT (a subscription the previous plan already carried under this
/// same token) outranks every newcomer. Without that, a saturated relay
/// would evict whatever the newest atom outranked, re-admit it next compile,
/// and oscillate forever: a CLOSE plus a REQ per recompile, caused by the
/// budget itself. Ranking incumbents first makes a bound budget quiet — the
/// relay keeps serving what it is already serving, and the demand that
/// cannot fit is refused explicitly rather than swapped in and out.
///
/// Below incumbency, the rank is the same coverage bias
/// [`apply_global_relay_cap`] uses (most absorbed atoms, then most route
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
            .then_with(|| b.absorbed.len().cmp(&a.absorbed.len()))
            .then_with(|| b.provenance.len().cmp(&a.provenance.len()))
            .then_with(|| a.filter.hash().cmp(&b.filter.hash()))
    });
    let refused = ranked.split_off(allowed.min(ranked.len()));
    // Restore the plan's own ordering invariant: `reqs` is sorted by
    // `SubId`, and `diff_plans` reads it back that way.
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
    source: &SourceAuthority,
    access: AccessContext,
    routes: Vec<(RelayUrl, RouteProvenance)>,
) {
    if routes.is_empty() {
        return;
    }
    let key = coverage_key(&ContextualAtom {
        filter: filter.clone(),
        source: source.clone(),
        access,
        routing_evidence: BTreeSet::new(),
    });
    for (relay, prov) in routes {
        bag.entry(RelaySessionKey::new(relay, access))
            .or_default()
            .entry(source.clone())
            .or_default()
            .push((filter.clone(), vec![prov], BTreeSet::from([key])));
    }
}

pub struct Router {
    rules: RuleRegistry,
    prev_plan: RelayPlan,
    last_diag: Diagnostics,
    /// Monotonic wire-token mint counter (#899). Never reset and never
    /// rewound, so no token is recycled within this `Router`'s lifetime.
    /// Deliberately NOT seeded randomly: the whole crate is a pure function of
    /// its inputs and the repo pins that reproducibility, while token
    /// uniqueness only ever has to hold WITHIN one router's wire namespace.
    next_token: u64,
    /// Hoisted chain root for [`SubId::allocate`], so minting costs a handful
    /// of `fold_byte` calls rather than a JSON encode plus BLAKE3 each time.
    /// Carries no filter meaning -- see `SubId::allocate`.
    mint_root: nmp_grammar::DescriptorHash,
}

impl Router {
    pub fn new(rules: RuleRegistry) -> Self {
        Self {
            rules,
            prev_plan: RelayPlan::default(),
            last_diag: Diagnostics::default(),
            next_token: 0,
            mint_root: ConcreteFilter::default().hash(),
        }
    }

    /// THE entry point. Recompile the whole per-relay plan from `demand`,
    /// diff vs the previous plan, store the new plan + diagnostics, return
    /// the surgical wire delta.
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
    ) -> WireDelta {
        let budget = budget.into();
        // Step 1: group demand by (Skeleton, AccessContext) (outbox) /
        // classify pinned -- classification is now by DECLARED
        // `SourceAuthority` (#106), never by filter shape alone. Grouping by
        // `AccessContext` alongside the skeleton keeps the seam ready for a
        // future non-`Public` access variant (#8's NIP-42 AUTH) without
        // needing a second widening later; every atom reaching this branch
        // shares `source: AuthorOutboxes` by construction (that's the
        // `classify` arm that produced it), so it isn't tracked per-group.
        let mut outbox_groups: BTreeMap<
            (Skeleton, AccessContext),
            BTreeMap<PublicKey, BTreeSet<RoutingEvidence>>,
        > = BTreeMap::new();
        let mut supplemental_atoms: BTreeMap<
            (ConcreteFilter, AccessContext),
            (BTreeSet<PublicKey>, BTreeSet<RoutingEvidence>),
        > = BTreeMap::new();
        // #107: query-declared `SourceAuthority::Pinned(relays)` atoms — kept
        // in their OWN collection, never merged into `pinned_atoms` (the
        // directory-fact `Public`-sourced kind), since these must skip every
        // additive lane (indexer/app/fallback) below, not just the solve.
        let mut exact_atoms: Vec<(ConcreteFilter, AccessContext, BTreeSet<RelayUrl>)> = Vec::new();
        for atom in demand {
            match route::classify(&atom.filter, &atom.source) {
                AtomClass::Coverage { skeleton, authors } => {
                    let group = outbox_groups.entry((skeleton, atom.access)).or_default();
                    for author in authors {
                        group
                            .entry(author)
                            .or_default()
                            .extend(atom.routing_evidence.iter().cloned());
                    }
                }
                AtomClass::Supplemental { authors } => {
                    let (known_authors, evidence) = supplemental_atoms
                        .entry((atom.filter.clone(), atom.access))
                        .or_default();
                    known_authors.extend(authors);
                    evidence.extend(atom.routing_evidence.iter().cloned());
                }
                AtomClass::Exact(relays) => {
                    exact_atoms.push((atom.filter.clone(), atom.access, relays));
                }
            }
        }

        // Step 2 + 3: route (coverage-solve outbox groups / pinned lookup),
        // apply the additive indexer/app/fallback lanes OUTSIDE the solve
        // (Unit B, `routing-and-ownership.md` §2.1/§2.2 — never counted
        // toward `k`), and materialize each relay's bag of (filter,
        // context, single-lane provenance, absorbed) entries. `absorbed` is
        // the coverage-attribution ruling's per-atom `CoverageKey` (§2):
        // each entry here is exactly one pre-coalesce demand atom (one
        // author, for outbox; the full/shortfall author set, for an
        // additive lane; the pinned atom itself, for pinned), so it
        // contributes exactly one key, later unioned by `coalesce_with`
        // alongside provenance as same-skeleton, SAME-CONTEXT atoms merge
        // (Fable D: equal-context-only).
        let mut bag: SessionBag = BTreeMap::new();
        let mut uncovered_authors: BTreeMap<PublicKey, Shortfall> = BTreeMap::new();

        for ((skeleton, access), evidence_by_author) in &outbox_groups {
            let access = *access;
            let source = SourceAuthority::AuthorOutboxes;
            let authors: BTreeSet<PublicKey> = evidence_by_author.keys().copied().collect();
            let candidates = route::build_candidates(&authors, facts);
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
            uncovered_authors.extend(coverage.shortfall.clone());

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
            // merged or unmerged. The per-author keys are still absorbed so an
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
                let keys = relay_authors
                    .iter()
                    .map(|author| {
                        coverage_key(&ContextualAtom {
                            filter: skeleton.with_authors(BTreeSet::from([*author])),
                            source: source.clone(),
                            access,
                            routing_evidence: evidence_by_author
                                .get(author)
                                .cloned()
                                .unwrap_or_default(),
                        })
                    })
                    .collect::<BTreeSet<_>>();
                bag.entry(RelaySessionKey::new(relay, access))
                    .or_default()
                    .entry(source.clone())
                    .or_default()
                    .push((filter, provenance, keys));
            }

            // Operator app policy supplements the group's full author set.
            let additive = route::operator_app_routes(facts, &authors);
            push_routes(
                &mut bag,
                &skeleton.with_authors(authors.clone()),
                &source,
                access,
                additive,
            );

            // Additive fallback lane: routes exactly the shortfall authors,
            // iff no appRelay is configured. `Coverage.shortfall` above has
            // already recorded the shortfall regardless of whether this
            // lane fires — fallback is a lane, not coverage.
            let shortfall_authors: BTreeSet<PublicKey> =
                coverage.shortfall.keys().copied().collect();
            let fallback = route::operator_fallback_routes(facts, &shortfall_authors);
            push_routes(
                &mut bag,
                &skeleton.with_authors(shortfall_authors),
                &source,
                access,
                fallback,
            );
        }

        for ((atom, access), (authors, routing_evidence)) in &supplemental_atoms {
            let source = SourceAuthority::Public;
            let access = *access;
            let key = coverage_key(&ContextualAtom {
                filter: atom.clone(),
                source: source.clone(),
                access,
                routing_evidence: routing_evidence.clone(),
            });
            let routes = route::provenance_for_projected(routing_evidence);
            for (relay, prov) in routes {
                bag.entry(RelaySessionKey::new(relay, access))
                    .or_default()
                    .entry(source.clone())
                    .or_default()
                    .push((atom.clone(), vec![prov], BTreeSet::from([key])));
            }

            // App lane routes every atom, including authorless/pinned ones
            // (closes #7 — the authorless-routing-lane gap).
            let app = route::operator_app_routes(facts, authors);
            push_routes(&mut bag, atom, &source, access, app);
        }

        // #107: explicit, query-declared pinned wire authority — route
        // DIRECTLY to the Demand's own relay set. NO additive lane
        // (indexer/app/fallback) is ever applied here: that's the #107
        // Contract's core guarantee ("Pinned author filters never contact
        // directory, author-outbox, app, fallback, or indexer relays").
        for (filter, access, relays) in &exact_atoms {
            let source = SourceAuthority::Pinned(relays.clone());
            let key = coverage_key(&ContextualAtom {
                filter: filter.clone(),
                source: source.clone(),
                access: *access,
                routing_evidence: BTreeSet::new(),
            });
            for (relay, prov) in route::provenance_for_exact(relays) {
                bag.entry(RelaySessionKey::new(relay, *access))
                    .or_default()
                    .entry(source.clone())
                    .or_default()
                    .push((filter.clone(), vec![prov], BTreeSet::from([key])));
            }
        }

        // Step 4: enforce the ONE whole-demand ceiling over the fully
        // materialized bag. Nothing removed here can reach coalescing, the
        // plan, or the wire; its contextual coverage keys remain as exact
        // local-limit evidence.
        let (mut limited, mut refused_sessions) =
            apply_global_relay_cap(&mut bag, budget.relay_cap(), &mut uncovered_authors);

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
        // Allocation buys back injectivity AND keeps in-place widening,
        // because the previous plan is the state that tells "this
        // subscription churned" apart from "a sibling appeared".
        let mint_root = self.mint_root;
        let mut mint_counter = self.next_token;
        let mut reqs: BTreeMap<RelaySessionKey, Vec<WireReq>> = BTreeMap::new();
        let mut subscription_shortfalls: BTreeMap<RelaySessionKey, BudgetShortfall> =
            BTreeMap::new();
        for (session, by_source) in bag {
            let relay = session.relay.clone();
            let access = session.access;
            let mut session_reqs: Vec<WireReq> = Vec::new();
            for (source, entries) in by_source {
                let merged = self.rules.coalesce_with(entries);
                let filters: Vec<ConcreteFilter> =
                    merged.iter().map(|(filter, _, _)| filter.clone()).collect();

                // The matching partition: this exact relay session AND this
                // exact declared authority. Reading it back out of
                // `prev_plan` is what makes the matching state pruned by
                // construction -- there is no separate table to age out.
                let priors: Vec<(ConcreteFilter, SubId)> = self
                    .prev_plan
                    .reqs
                    .get(&session)
                    .into_iter()
                    .flatten()
                    .filter(|req| req.source == source)
                    .map(|req| (req.filter.clone(), req.sub_id.clone()))
                    .collect();

                let assigned = wire_id::assign(&priors, &filters, || {
                    let sub_id =
                        SubId::allocate(relay.clone(), &source, access, mint_root, mint_counter);
                    mint_counter += 1;
                    sub_id
                });

                session_reqs.extend(merged.into_iter().zip(assigned).map(
                    |((filter, provenance, absorbed), sub_id)| WireReq {
                        sub_id,
                        filter,
                        source: source.clone(),
                        provenance,
                        absorbed,
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
                    let refused = refuse_over_budget(
                        &mut session_reqs,
                        allowed,
                        self.prev_plan.reqs.get(&session),
                    );
                    for req in &refused {
                        limited.extend(req.absorbed.iter().copied());
                    }
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
        self.next_token = mint_counter;

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

        let next_plan = RelayPlan {
            reqs,
            limited,
            refused_sessions,
            subscription_shortfalls,
        };

        // Step 7: diff vs previous plan.
        let delta = diff_plans(&self.prev_plan, &next_plan);

        self.last_diag = diag::build(
            &next_plan,
            &budget,
            uncovered_authors,
            self.rules.dropped_rules().to_vec(),
        );
        self.prev_plan = next_plan;
        delta
    }

    pub fn diagnostics(&self) -> &Diagnostics {
        &self.last_diag
    }

    pub fn plan(&self) -> &RelayPlan {
        &self.prev_plan
    }
}
