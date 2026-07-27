//! `RelayPlan` + the wire delta + plan diffing (M2 plan §2.5).

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::{
    fold_byte, fold_context, AccessContext, ConcreteFilter, DescriptorHash, RelaySessionKey,
    SourceAuthority,
};
use nmp_store::CoverageKey;

use crate::facts::RelayUrl;
use crate::route::{RouteProvenance, Skeleton};

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
pub type SkeletonHash = DescriptorHash;

/// Domain byte separating an ALLOCATED token's derivation from every DERIVED
/// id sharing this type. Folded in before the counter, so an allocated token
/// can never coincide with a `Skeleton`-derived one ([`SubId::for_wire`], the
/// prober's namespace) even in principle.
const ALLOCATED_DOMAIN: u8 = 0xa1;

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct SubId(pub RelayUrl, pub SkeletonHash, pub AccessContext);

impl SubId {
    /// Mint a FRESH, never-before-used wire token for `relay` under
    /// `source`/`access`.
    ///
    /// `counter` is the router's own monotonic mint counter, so no token is
    /// ever recycled within a `Router`'s lifetime — reuse would let a stale
    /// in-flight EOSE for a closed subscription land on a reopened one's
    /// attribution FIFO. A process restart is safe without any persistence:
    /// connections drop, and `nmp-engine`'s `AttributionState::clear_session`
    /// wipes every stale wire mapping for the session.
    ///
    /// `root` is a caller-supplied chain root (the router hoists
    /// `ConcreteFilter::default().hash()` once, rather than paying a JSON
    /// encode plus BLAKE3 per mint). It carries no filter meaning: the token's
    /// uniqueness comes entirely from `counter`, and `root` merely gives the
    /// `fold_byte` chain a `DescriptorHash` to start from without this crate
    /// taking a direct `blake3` dependency.
    ///
    /// `fold_context` is applied LAST, exactly as [`Self::for_wire`] does, so
    /// the #106 anti-alias property survives allocation: identical relay +
    /// filter under different [`SourceAuthority`] can never share a token,
    /// belt-and-braces on top of the assignment's own injectivity.
    ///
    /// Deliberately NOT derived from anything mutable the relay advertises:
    /// no NIP-11 field (`max_subid_length` and friends) feeds this, so a relay
    /// changing its advertisement can never move an established id.
    pub(crate) fn allocate(
        relay: RelayUrl,
        source: &SourceAuthority,
        access: AccessContext,
        root: DescriptorHash,
        counter: u64,
    ) -> Self {
        let mut hash = fold_byte(root, ALLOCATED_DOMAIN);
        for byte in counter.to_be_bytes() {
            hash = fold_byte(hash, byte);
        }
        SubId(relay, fold_context(hash, source, access), access)
    }

    /// DERIVE a sub-id for `filter` on `relay` from the filter's OWN skeleton
    /// (authors erased) folded with its [`SourceAuthority`]/[`AccessContext`]
    /// (#106, atlas's 3rd proof floor).
    ///
    /// **This is NO LONGER how planned subscriptions are identified.** The
    /// plan allocates opaque tokens instead ([`Self::allocate`]), because
    /// erasing `authors` from the identity is exactly what let two filters
    /// the coalescer REFUSED to merge collide onto one subscription, with
    /// `diff_plans` then silently dropping one of them (#899). Erasure bought
    /// author-churn stability without previous-plan state; it paid for it
    /// with injectivity, and injectivity is not optional.
    ///
    /// What still uses this: the negentropy PROBER
    /// (`nmp-engine::negentropy::Prober::begin_probe`), which mints protocol-
    /// support probe ids into its own `pending` map and never touches
    /// coverage or attribution identity. That namespace is domain-separated
    /// from allocated tokens by [`ALLOCATED_DOMAIN`]. Folding context in is
    /// what keeps two DIFFERENT-context atoms sharing a relay+skeleton from
    /// colliding onto the SAME sub-id — doing so would re-alias their
    /// inflight attribution FIFO (`nmp-engine::core::attribution
    /// ::AttributionState`) exactly the way the per-context `CoverageKey`
    /// widening was built to prevent.
    pub fn for_wire(
        relay: RelayUrl,
        filter: &ConcreteFilter,
        source: &SourceAuthority,
        access: AccessContext,
    ) -> Self {
        let (skeleton, _) = Skeleton::of(filter);
        SubId(relay, fold_context(skeleton.hash(), source, access), access)
    }
}

/// A single wire request: the (possibly coalesced/widened) filter plus why
/// it exists.
///
/// `absorbed` (coverage-attribution ruling
/// `docs/consults/2026-07-11-fable-coverage-attribution.md` §2) is every
/// narrow demand atom's window-erased [`CoverageKey`] this (possibly
/// coalesced) wire filter supersets — populated at materialization (one key
/// per pre-coalesce atom entry) and concatenated through every
/// `coalesce_with` merge exactly as `provenance` already is. Because every
/// merge in this crate is widen-only-proven (`coalesce.rs`), `wide ⊇ atom`
/// holds for every key in `absorbed` BY CONSTRUCTION at the moment of
/// materialization — this is the containment rule the ruling requires,
/// discharged once, here, never re-derived at read time by subset-testing
/// filters (banned by the ruling).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct WireReq {
    pub sub_id: SubId,
    pub filter: ConcreteFilter,
    /// The declared wire authority this req was routed under. `SubId` already
    /// carries the relay and the [`AccessContext`]; this is the missing half
    /// of the identity context, and it is what makes the previous plan
    /// re-partitionable for signature matching (`crate::wire_id`) — a
    /// `Public`-sourced filter must never inherit an `AuthorOutboxes`-sourced
    /// filter's token, which is the wire-side half of the #106 anti-alias.
    pub source: SourceAuthority,
    pub provenance: Vec<RouteProvenance>,
    pub absorbed: BTreeSet<CoverageKey>,
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
    /// `planned - budget`: subscriptions removed, every one of them also
    /// reported through `RelayPlan::limited`.
    pub refused: usize,
}

/// The full per-relay plan for the CURRENT demand set.
#[derive(Clone, Default, Debug)]
pub struct RelayPlan {
    pub reqs: BTreeMap<RelaySessionKey, Vec<WireReq>>,
    /// Narrow demand atoms for which a local bound removed at least one
    /// otherwise-routable source — the whole-demand relay ceiling, or a
    /// relay's own advertised concurrent-subscription budget (#931). Kept as
    /// coverage keys so the engine can join the fact back to the exact
    /// contextual atom without weakening descriptor identity.
    ///
    /// This is the seam that makes a bound budget impossible to mistake for
    /// a complete acquisition: `plan_is_fresh_for` refuses to call a limited
    /// atom fresh, and `acquisition_evidence` reports it to the app as
    /// `ShortfallFact::LocalLimit`.
    pub limited: BTreeSet<CoverageKey>,
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

/// A single wire operation. `Req` is open-or-replace (same sub-id
/// overwrites); `Close` withdraws a sub-id.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum WireOp {
    Req(SubId, ConcreteFilter),
    Close(SubId),
}

/// Surgical per-relay deltas — the M1 atom-diffing discipline lifted to the
/// wire layer. INVARIANT (mirrors `DemandDelta`): within each relay's op
/// list, all `Close` ops precede all `Req` ops.
#[derive(Clone, Default, Debug)]
pub struct WireDelta {
    pub ops: Vec<(RelaySessionKey, Vec<WireOp>)>,
}

/// Diff `next` against `prev`. Unchanged (relay, skeleton) subs whose
/// filter is byte-identical emit NOTHING (the relay does not appear in the
/// output at all); a changed filter on an existing sub emits one
/// `Req(sub_id, new)`; a vanished sub emits `Close(sub_id)`; a new sub
/// emits `Req(sub_id, filter)`.
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

    fn cf(kind: u16, authors: &[&str]) -> ConcreteFilter {
        ConcreteFilter {
            kinds: Some(BTreeSet::from([kind])),
            authors: Some(authors.iter().map(|s| s.to_string()).collect()),
            ..ConcreteFilter::default()
        }
    }

    fn relay(n: usize) -> RelayUrl {
        crate::facts::test_relay(n)
    }

    fn plan_of(relay: RelayUrl, filter: ConcreteFilter) -> RelayPlan {
        let sub_id = SubId::for_wire(
            relay.clone(),
            &filter,
            &SourceAuthority::AuthorOutboxes,
            AccessContext::Public,
        );
        let req = WireReq {
            sub_id,
            filter,
            source: SourceAuthority::AuthorOutboxes,
            provenance: Vec::new(),
            absorbed: BTreeSet::new(),
        };
        RelayPlan {
            reqs: BTreeMap::from([(RelaySessionKey::public(relay), vec![req])]),
            ..RelayPlan::default()
        }
    }

    #[test]
    fn identical_plans_diff_to_nothing() {
        let plan = plan_of(relay(0), cf(1, &["aa"]));
        let delta = diff_plans(&plan, &plan.clone());
        assert!(delta.ops.is_empty());
    }

    #[test]
    fn author_churn_same_skeleton_emits_one_overwriting_req() {
        let prev = plan_of(relay(0), cf(1, &["aa", "bb"]));
        let next = plan_of(relay(0), cf(1, &["aa", "cc"]));
        let delta = diff_plans(&prev, &next);
        assert_eq!(delta.ops.len(), 1);
        let (r, ops) = &delta.ops[0];
        assert_eq!(r, &RelaySessionKey::public(relay(0)));
        assert_eq!(ops.len(), 1);
        assert!(
            matches!(&ops[0], WireOp::Req(_, f) if f.authors == Some(BTreeSet::from(["aa".to_string(), "cc".to_string()])))
        );
    }

    #[test]
    fn vanished_sub_emits_close_new_sub_emits_req() {
        let prev = plan_of(relay(0), cf(1, &["aa"]));
        let next = plan_of(relay(0), cf(2, &["aa"]));
        let delta = diff_plans(&prev, &next);
        assert_eq!(delta.ops.len(), 1);
        let (_, ops) = &delta.ops[0];
        assert_eq!(ops.len(), 2);
        assert!(matches!(ops[0], WireOp::Close(_)));
        assert!(matches!(ops[1], WireOp::Req(_, _)));
    }

    #[test]
    fn untouched_relay_never_appears_in_delta() {
        let mut prev = plan_of(relay(0), cf(1, &["aa"]));
        let next_only = plan_of(relay(1), cf(1, &["bb"]));
        prev.reqs.extend(next_only.reqs.clone());

        let mut next = prev.clone();
        // Change relay 1's filter only.
        next.reqs.insert(
            RelaySessionKey::public(relay(1)),
            vec![WireReq {
                sub_id: SubId::for_wire(
                    relay(1),
                    &cf(1, &["bb", "cc"]),
                    &SourceAuthority::AuthorOutboxes,
                    AccessContext::Public,
                ),
                filter: cf(1, &["bb", "cc"]),
                source: SourceAuthority::AuthorOutboxes,
                provenance: Vec::new(),
                absorbed: BTreeSet::new(),
            }],
        );

        let delta = diff_plans(&prev, &next);
        assert_eq!(delta.ops.len(), 1);
        assert_eq!(delta.ops[0].0, RelaySessionKey::public(relay(1)));
    }

    /// #106/atlas's 3rd proof floor: the identical relay+filter under
    /// DIFFERENT `SourceAuthority` must mint DIFFERENT `SubId`s. Before this
    /// fix, `SubId::for_filter` keyed purely on (relay, skeleton), so two
    /// distinct-context atoms sharing a filter would collapse onto ONE
    /// inflight attribution FIFO (`nmp-engine::core::attribution`),
    /// crediting one context's EOSE to the other's `AcquisitionEvidence` --
    /// the wire-layer twin of the store-side `CoverageKey` anti-alias.
    #[test]
    fn for_wire_distinguishes_identical_filters_under_different_source_authority() {
        let filter = cf(1, &["aa"]);
        let outbox_sub = SubId::for_wire(
            relay(0),
            &filter,
            &SourceAuthority::AuthorOutboxes,
            AccessContext::Public,
        );
        let public_sub = SubId::for_wire(
            relay(0),
            &filter,
            &SourceAuthority::Public,
            AccessContext::Public,
        );
        assert_ne!(
            outbox_sub, public_sub,
            "identical relay+filter under different SourceAuthority must never share a SubId"
        );
    }

    /// Author churn under a FIXED context still reuses the same `SubId`
    /// (the property `for_wire`'s doc promises is unchanged by folding in
    /// context) -- context-folding widens WHAT distinguishes two subs, it
    /// never narrows the existing skeleton-stability guarantee.
    #[test]
    fn for_wire_author_churn_same_context_reuses_sub_id() {
        let a = cf(1, &["aa", "bb"]);
        let b = cf(1, &["aa", "cc"]);
        let sub_a = SubId::for_wire(
            relay(0),
            &a,
            &SourceAuthority::AuthorOutboxes,
            AccessContext::Public,
        );
        let sub_b = SubId::for_wire(
            relay(0),
            &b,
            &SourceAuthority::AuthorOutboxes,
            AccessContext::Public,
        );
        assert_eq!(sub_a, sub_b);
    }
}
