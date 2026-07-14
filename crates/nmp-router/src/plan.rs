//! `RelayPlan` + the wire delta + plan diffing (M2 plan §2.5).

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::{fold_context, AccessContext, ConcreteFilter, DescriptorHash, SourceAuthority};
use nmp_store::CoverageKey;

use crate::facts::RelayUrl;
use crate::route::{RouteProvenance, Skeleton};

/// A stable subscription id, keyed by (relay, skeleton) so that
/// adding/removing an author re-uses the same sub-id: on the wire that is
/// ONE overwriting REQ, not close+reopen of every author (NIP-01: a REQ
/// with an existing sub-id replaces that sub's filter).
pub type SkeletonHash = DescriptorHash;

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct SubId(pub RelayUrl, pub SkeletonHash);

impl SubId {
    /// Derive the sub-id for `filter` on `relay` from the filter's OWN
    /// skeleton (authors erased) folded with its [`SourceAuthority`]/
    /// [`AccessContext`] (#106, atlas's 3rd proof floor). An `AuthorUnion`
    /// merge never changes a filter's skeleton (it only touches `authors`),
    /// so this is automatically stable across author churn without any
    /// external bookkeeping; a `KindUnion` merge produces a new, but still
    /// deterministically reproducible, skeleton. Folding in context is what
    /// keeps two DIFFERENT-context atoms sharing a relay+skeleton from
    /// colliding onto the SAME sub-id: under equal-context-only coalescing
    /// (Fable D) they never coalesce into one wire filter, so they must not
    /// collapse onto one `SubId` either — doing so would re-alias their
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
        SubId(relay, fold_context(skeleton.hash(), source, access))
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
    pub provenance: Vec<RouteProvenance>,
    pub absorbed: BTreeSet<CoverageKey>,
}

/// The full per-relay plan for the CURRENT demand set.
#[derive(Clone, Default, Debug)]
pub struct RelayPlan {
    pub reqs: BTreeMap<RelayUrl, Vec<WireReq>>,
    /// Narrow demand atoms for which the whole-demand relay ceiling removed
    /// at least one otherwise-routable source. Kept as coverage keys so the
    /// engine can join the fact back to the exact contextual atom without
    /// weakening descriptor identity.
    pub limited: BTreeSet<CoverageKey>,
    /// Distinct relay candidates refused by the whole-demand ceiling. This
    /// is diagnostics evidence, not a second routing input: only `reqs` may
    /// reach the wire.
    pub refused_relays: BTreeSet<RelayUrl>,
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
    pub ops: Vec<(RelayUrl, Vec<WireOp>)>,
}

/// Diff `next` against `prev`. Unchanged (relay, skeleton) subs whose
/// filter is byte-identical emit NOTHING (the relay does not appear in the
/// output at all); a changed filter on an existing sub emits one
/// `Req(sub_id, new)`; a vanished sub emits `Close(sub_id)`; a new sub
/// emits `Req(sub_id, filter)`.
pub fn diff_plans(prev: &RelayPlan, next: &RelayPlan) -> WireDelta {
    let relays: BTreeSet<&RelayUrl> = prev.reqs.keys().chain(next.reqs.keys()).collect();
    let mut ops = Vec::new();

    for relay in relays {
        let prev_by_sub: BTreeMap<&SubId, &ConcreteFilter> = prev
            .reqs
            .get(relay)
            .into_iter()
            .flatten()
            .map(|r| (&r.sub_id, &r.filter))
            .collect();
        let next_by_sub: BTreeMap<&SubId, &ConcreteFilter> = next
            .reqs
            .get(relay)
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
        ops.push((relay.clone(), relay_ops));
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
            provenance: Vec::new(),
            absorbed: BTreeSet::new(),
        };
        RelayPlan {
            reqs: BTreeMap::from([(relay, vec![req])]),
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
        assert_eq!(r, &relay(0));
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
            relay(1),
            vec![WireReq {
                sub_id: SubId::for_wire(
                    relay(1),
                    &cf(1, &["bb", "cc"]),
                    &SourceAuthority::AuthorOutboxes,
                    AccessContext::Public,
                ),
                filter: cf(1, &["bb", "cc"]),
                provenance: Vec::new(),
                absorbed: BTreeSet::new(),
            }],
        );

        let delta = diff_plans(&prev, &next);
        assert_eq!(delta.ops.len(), 1);
        assert_eq!(delta.ops[0].0, relay(1));
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
