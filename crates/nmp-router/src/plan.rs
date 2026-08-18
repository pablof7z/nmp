//! `RelayPlan` + the wire delta + plan diffing (M2 plan §2.5).

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::{
    ConcreteFilter, ContextualAtom,
    ReadRouting, RelaySessionKey,
};
use nmp_store::{coverage_key, CoverageKey};

use crate::facts::RelayUrl;
use crate::route::RouteProvenance;

/// The 256-bit digest a [`SubId`] carries as its wire identity. `EngineCore`
/// sends every REQ under this value's hex `Display` (64 characters — exactly
/// NIP-01's `subscription_id` cap, never prefixed or truncated).
///
/// Two DIFFERENT kinds of value inhabit this type, and the difference is the
/// whole of #899:
///
/// - For a **planned** `WireReq`, it is an ALLOCATED OPAQUE TOKEN
///   ([`SubId::allocate`]) — minted at a filter's first appearance and carried
///   forward across recompiles by structural-signature matching
///   ([`crate::wire_id`]). It is NOT a function of the filter, and nothing may
///   reconstruct it from one.
/// - For an id DERIVED outside the plan — the negentropy prober
///   (`nmp-engine::negentropy`) and the NIP-77 role ids folded off a plan
///   token — it is still a content hash, in its own namespace.
///
/// The name is historical. It used to be exactly what it says: the hash of
/// the filter's author-erased [`Skeleton`], which is precisely why two filters
/// differing only in `authors` collided onto one subscription (#899).
/// A wire subscription token: the router's own monotonic mint counter.
///
/// NOT derived from the filter. NIP-01 lets `subscription_id` be any string
/// up to 64 characters, and a token's job is to be a NAME the relay echoes
/// back, not a fingerprint of what was asked. Deriving it from filter
/// content bought nothing -- uniqueness came from the counter either way --
/// while making every mint pay a canonical JSON encode plus a BLAKE3, and
/// making the id move whenever the filter moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WireToken {
    /// The router's monotonic mint counter. Sole source of uniqueness.
    pub mint: u64,
    /// NIP-77 role, 0 for an ordinary REQ. Carried as a FIELD rather than
    /// folded into a digest, so the wire id can be read back.
    pub role: u8,
    /// NIP-77 reconciliation incarnation, 0 for an ordinary REQ.
    pub incarnation: u64,
}

impl WireToken {
    pub fn new(mint: u64) -> Self {
        Self { mint, role: 0, incarnation: 0 }
    }

    /// A role-scoped sibling of this token. Same mint, distinct wire id.
    pub fn with_role(self, role: u8, incarnation: u64) -> Self {
        Self { mint: self.mint, role, incarnation }
    }
}

impl std::fmt::Display for WireToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.role == 0 && self.incarnation == 0 {
            write!(f, "{}", self.mint)
        } else {
            write!(f, "{}-{}-{}", self.mint, self.role, self.incarnation)
        }
    }
}

/// Exact application demand identity for relay admission and withdrawal.
///
/// Durable [`CoverageKey`] deliberately erases `since`, `until`, and
/// `limit`, because those values describe the interval proven by EOSE rather
/// than a different durable selection. Relay lifecycle cannot erase them: an
/// already-running live request does not backfill a newly-requested older
/// page, and a bounded request is not interchangeable with an unbounded one.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct DemandKey {
    coverage: CoverageKey,
    since: Option<u64>,
    until: Option<u64>,
    limit: Option<usize>,
}

impl DemandKey {
    pub fn for_atom(atom: &ContextualAtom) -> Self {
        Self::from_filter(coverage_key(atom), &atom.filter)
    }

    pub(crate) fn from_filter(coverage: CoverageKey, filter: &ConcreteFilter) -> Self {
        Self {
            coverage,
            since: filter.since,
            until: filter.until,
            limit: filter.limit,
        }
    }

    pub fn coverage(&self) -> CoverageKey {
        self.coverage.clone()
    }
}

/// Domain byte separating an ALLOCATED token's derivation from every DERIVED
/// id sharing this type. Folded in before the counter, so an allocated token
/// can never coincide with a `Skeleton`-derived one ([`SubId::for_wire`], the
/// prober's namespace) even in principle.

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct SubId(pub RelayUrl, pub WireToken, pub Option<nostr::PublicKey>);

impl SubId {
    /// Mint a FRESH, never-before-used wire token for `relay` under this
    /// authenticated identity.
    ///
    /// `counter` is the router's own monotonic mint counter, so no token is
    /// ever recycled within a `Router`'s lifetime -- reuse would let a stale
    /// in-flight EOSE for a closed subscription land on a reopened one's
    /// attribution FIFO. A process restart is safe without any persistence:
    /// connections drop, and `nmp-engine`'s `AttributionState::clear_session`
    /// wipes every stale wire mapping for the session.
    ///
    /// The token is the counter and nothing else. It carries no filter
    /// meaning, no routing meaning and no identity meaning -- those live in
    /// the `SubId`'s other two fields, where they can be READ rather than
    /// recovered from a digest. The plan's injectivity comes from the
    /// assignment in `wire_id::assign`, not from any property of the token.
    pub fn allocate(
        relay: RelayUrl,
        _source: &ReadRouting,
        authenticate_as: Option<nostr::PublicKey>,
        counter: u64,
    ) -> Self {
        SubId(relay, WireToken::new(counter), authenticate_as)
    }

}

/// A single wire request: the (possibly coalesced/widened) filter plus why
/// it exists.
///
/// `coverage_claims` (coverage-attribution ruling
/// `docs/design/query-demand-and-evidence.md`) is every
/// narrow demand atom's window-erased [`CoverageKey`] this (possibly
/// coalesced) wire filter supersets — populated at materialization (one key
/// per pre-coalesce atom entry) and concatenated through every
/// `coalesce_with` merge exactly as `provenance` already is. Because every
/// merge in this crate is widen-only-proven (`coalesce.rs`), `wide ⊇ atom`
/// holds for every key in `coverage_claims` BY CONSTRUCTION at the moment of
/// materialization — this is the containment rule the ruling requires,
/// discharged once, here, never re-derived at read time by subset-testing
/// filters (banned by the ruling).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct WireReq {
    pub sub_id: SubId,
    pub filter: ConcreteFilter,
    /// The declared routing this req was compiled under. `SubId` already
    /// carries the relay and the authenticated identity; this is the missing half
    /// of the identity context, and it is what makes the previous plan
    /// re-partitionable for signature matching (`crate::wire_id`) — an
    /// `Auto`-routed filter must never inherit an `Explicit`-routed filter's
    /// token, which is the wire-side half of the #106 anti-alias.
    pub routing: ReadRouting,
    pub provenance: BTreeSet<RouteProvenance>,
    /// Exact durable coverage-claim keys carried independently of the
    /// synthetic/coalesced wire filter. Core registers each normalized shape
    /// once through `nmp_store::coverage_claim_atoms`; requests retain only
    /// the compact immutable keys.
    pub coverage_claims: BTreeSet<CoverageKey>,
    /// Exact current local demand identities attached to this immutable
    /// physical request. Unlike coverage claims, these retain the request
    /// window/count. Local metadata may shrink on an exact owner 1->0 while
    /// the sent filter/subscription id remain byte-identical.
    pub owner_demands: BTreeSet<DemandKey>,
    /// Exact k-of-n assignments this request actually serves. Supplemental
    /// app/fallback lanes own request lifecycle but never count as an author
    /// outbox coverage assignment.
    pub coverage_assignments: BTreeSet<(DemandKey, crate::facts::PublicKey)>,
}

/// One session's per-relay subscription-budget shortfall (#931): what the
/// relay advertised, what the compile wanted, and how much of it was refused.
///
/// A shortfall is recorded ONLY when the budget actually bound. Its absence
/// is the normal case and means "everything planned reached the wire" — not
/// "no budget applies", which [`crate::Diagnostics`] reports separately.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BudgetShortfall {
    /// The relay's own advertised `max_subscriptions`.
    pub budget: usize,
    /// Subscriptions this compile would have opened without the budget.
    pub planned: usize,
    /// `planned - budget`: subscriptions removed, with their exact logical
    /// owners reported through [`RelayPlan::limited_demands`].
    pub refused: usize,
}

/// The full per-relay plan for the CURRENT demand set.
///
/// `PartialEq` is what "the plan did not change" is spelled with. Ten
/// falsifiers in `nmp-engine` made that claim before this derive existed, and
/// each had to hand-pick a subset to compare — one request out of the only
/// session, one session's vector, the first ten thousand of a slice, or a
/// four-`assert_eq!` helper that compared `limited_demands` twice and would
/// silently not cover a fifth field added here (#1850). Every one of those
/// spellings passes while the router adds a request, refuses a session, or
/// records a shortfall. Comparing the whole value is both the exact claim and
/// the only one that stays exact as this struct grows.
#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct RelayPlan {
    pub reqs: BTreeMap<RelaySessionKey, Vec<WireReq>>,
    /// Exact demand identities for which a local bound removed at least one
    /// otherwise-routable source. Window/count boundaries remain part of the
    /// key so one limited demand can never contaminate a sibling sharing its
    /// durable coverage shape.
    pub limited_demands: BTreeSet<DemandKey>,
    /// Distinct relay candidates refused ENTIRELY — by the whole-demand
    /// ceiling, or by a relay advertising zero concurrent subscriptions.
    /// This is diagnostics evidence, not a second routing input: only `reqs`
    /// may reach the wire, and a refused session is absent from `reqs` by
    /// construction.
    ///
    /// A session merely TRIMMED by its subscription budget is NOT here: it
    /// is still planned and still serving, so it keeps its `reqs` row and
    /// reports through `subscription_shortfalls` instead.
    pub refused_sessions: BTreeSet<RelaySessionKey>,
    /// Per-session evidence for every relay whose advertised subscription
    /// budget actually bound this compile.
    pub subscription_shortfalls: BTreeMap<RelaySessionKey, BudgetShortfall>,
}

/// A single wire operation. `Req` opens the named subscription; Router-planned
/// byte changes use a fresh id and a typed replacement transition, while only
/// exact zero-diff retains an existing id. `Close` withdraws a sub-id after
/// the owning transition reaches its commit edge.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum WireOp {
    Req(SubId, ConcreteFilter),
    Close(SubId),
}

/// Surgical per-relay deltas — the M1 atom-diffing discipline lifted to the
/// wire layer. Raw canonical deltas list `Close` before `Req`; a
/// [`crate::CompileOutcome`] separately names byte-changing replacement pairs
/// so EngineCore withholds each predecessor close until its fresh-id successor
/// reaches the exact commit edge.
#[derive(Clone, Default, Debug)]
pub struct WireDelta {
    pub ops: Vec<(RelaySessionKey, Vec<WireOp>)>,
}

/// Diff `next` against `prev`. Unchanged (relay, skeleton) subs whose
/// filter is byte-identical emit NOTHING (the relay does not appear in the
/// output at all). A changed filter puts a predecessor `Close` plus fresh-id
/// successor `Req` in this raw delta; [`crate::CompileOutcome`] names that pair
/// so EngineCore offers the successor first and defers the close until exact
/// acceptance. A vanished sub emits `Close(sub_id)` and a new sub emits
/// `Req(sub_id, filter)`.
pub fn diff_plans(prev: &RelayPlan, next: &RelayPlan) -> WireDelta {
    let sessions: BTreeSet<&RelaySessionKey> = prev.reqs.keys().chain(next.reqs.keys()).collect();
    let mut ops = Vec::new();

    for session in sessions {
        let prev_by_sub: BTreeMap<&SubId, &ConcreteFilter> = prev
            .reqs
            .get(session)
            .into_iter()
            .flatten()
            .map(|r| (&r.sub_id, &r.filter))
            .collect();
        let next_by_sub: BTreeMap<&SubId, &ConcreteFilter> = next
            .reqs
            .get(session)
            .into_iter()
            .flatten()
            .map(|r| (&r.sub_id, &r.filter))
            .collect();

        let mut closes: Vec<SubId> = prev_by_sub
            .keys()
            .filter(|sub_id| !next_by_sub.contains_key(*sub_id))
            .map(|s| (*s).clone())
            .collect();
        closes.sort();

        let mut reqs: Vec<(SubId, ConcreteFilter)> = next_by_sub
            .iter()
            .filter(|(sub_id, filter)| prev_by_sub.get(*sub_id) != Some(*filter))
            .map(|(s, f)| ((*s).clone(), (*f).clone()))
            .collect();
        reqs.sort_by(|a, b| a.0.cmp(&b.0));

        if closes.is_empty() && reqs.is_empty() {
            continue;
        }

        let mut relay_ops: Vec<WireOp> = closes.into_iter().map(WireOp::Close).collect();
        relay_ops.extend(reqs.into_iter().map(|(s, f)| WireOp::Req(s, f)));
        ops.push((session.clone(), relay_ops));
    }

    WireDelta { ops }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn author(label: &str) -> String {
        label.repeat(32)
    }

    fn cf(kind: u16, authors: &[&str]) -> ConcreteFilter {
        ConcreteFilter {
            kinds: Some(BTreeSet::from([kind])),
            authors: Some(authors.iter().map(|label| author(label)).collect()),
            ..ConcreteFilter::default()
        }
    }

    fn relay(n: usize) -> RelayUrl {
        nmp_router_testkit::test_relay(n)
    }

    /// `token` names WHICH subscription this plan holds. It is explicit
    /// because a wire token is allocated, not derived: two plans share a
    /// token when they are the same subscription whose filter changed, and
    /// differ when one subscription went away and another appeared. That
    /// distinction used to be implicit in whether two filters happened to
    /// hash alike.
    fn plan_of(relay: RelayUrl, filter: ConcreteFilter, token: u64) -> RelayPlan {
        let sub_id = SubId::allocate(relay.clone(), &ReadRouting::Auto, None, token);
        let req = WireReq {
            sub_id,
            filter,
            routing: ReadRouting::Auto,
            provenance: BTreeSet::new(),
            coverage_claims: BTreeSet::new(),
            owner_demands: BTreeSet::new(),
            coverage_assignments: BTreeSet::new(),
        };
        RelayPlan {
            reqs: BTreeMap::from([(RelaySessionKey::unauthenticated(relay), vec![req])]),
            ..RelayPlan::default()
        }
    }

    #[test]
    fn identical_plans_diff_to_nothing() {
        let plan = plan_of(relay(0), cf(1, &["aa"]), 1);
        let delta = diff_plans(&plan, &plan.clone());
        assert!(delta.ops.is_empty());
    }

    #[test]
    fn author_churn_same_skeleton_emits_one_overwriting_req() {
        let prev = plan_of(relay(0), cf(1, &["aa", "bb"]), 1);
        let next = plan_of(relay(0), cf(1, &["aa", "cc"]), 1);
        let delta = diff_plans(&prev, &next);
        assert_eq!(delta.ops.len(), 1);
        let (r, ops) = &delta.ops[0];
        assert_eq!(r, &RelaySessionKey::unauthenticated(relay(0)));
        assert_eq!(ops.len(), 1);
        assert!(
            matches!(&ops[0], WireOp::Req(_, f) if f.authors == Some(BTreeSet::from([author("aa"), author("cc")])))
        );
    }

    #[test]
    fn vanished_sub_emits_close_new_sub_emits_req() {
        let prev = plan_of(relay(0), cf(1, &["aa"]), 1);
        let next = plan_of(relay(0), cf(2, &["aa"]), 2);
        let delta = diff_plans(&prev, &next);
        assert_eq!(delta.ops.len(), 1);
        let (_, ops) = &delta.ops[0];
        assert_eq!(ops.len(), 2);
        assert!(matches!(ops[0], WireOp::Close(_)));
        assert!(matches!(ops[1], WireOp::Req(_, _)));
    }

    #[test]
    fn untouched_relay_never_appears_in_delta() {
        let mut prev = plan_of(relay(0), cf(1, &["aa"]), 1);
        let next_only = plan_of(relay(1), cf(1, &["bb"]), 2);
        prev.reqs.extend(next_only.reqs.clone());

        let mut next = prev.clone();
        // Change relay 1's filter only.
        next.reqs.insert(
            RelaySessionKey::unauthenticated(relay(1)),
            vec![WireReq {
                sub_id: SubId::allocate(relay(1), &ReadRouting::Auto, None, 9),
                filter: cf(1, &["bb", "cc"]),
                routing: ReadRouting::Auto,
                provenance: BTreeSet::new(),
                coverage_claims: BTreeSet::new(),
                owner_demands: BTreeSet::new(),
                coverage_assignments: BTreeSet::new(),
            }],
        );

        let delta = diff_plans(&prev, &next);
        assert_eq!(delta.ops.len(), 1);
        assert_eq!(delta.ops[0].0, RelaySessionKey::unauthenticated(relay(1)));
    }

}
