//! Do the incrementally-maintained router indexes still say what a
//! from-scratch rebuild says?
//!
//! `admit` / `activate` / `withdraw` / `reactivate_covered_atom` / `compile`
//! each edit ~23 indexes in place. `Router::rebuild_active_indexes` derives
//! seventeen of the `Router`'s thirty fields from scratch out of
//! `(prev_plan.reqs, active_demands)`. The crate already asserts, at 49
//! sites, that everything DRAINS to `Default::default()` after teardown.
//! Draining is a weaker claim than agreeing: an index that gained a spurious
//! entry and then lost it again drains just fine.
//!
//! Every case here therefore stops at a NON-EMPTY intermediate state and
//! compares the incremental router against a twin rebuilt from the same
//! `(rules, prev_plan, active_demands)`.
//!
//! ## What a rebuild can and cannot re-derive
//!
//! Thirteen of the seventeen ARE a pure function of the running plan plus the
//! active demand set, and every case below asserts them equal.
//!
//! Four are NOT, and no sequence can make them so:
//! `physical_claims_by_request`, `requests_by_physical_claim`,
//! `physical_contributions_by_request` and `requests_by_physical_demand`
//! record what a request owned when it ENTERED the plan. `WireReq` carries
//! only what it owns NOW, and the two diverge in both directions:
//!
//! - a local metadata attach (`attach_exact_request_metadata`) adds owners
//!   and claims to `WireReq` while deliberately leaving the physical sidecar
//!   alone, so a rebuild over-counts;
//! - a partial withdrawal prunes owners and claims off `WireReq` while the
//!   physical edges survive until the request physically closes, so a rebuild
//!   under-counts.
//!
//! Both asymmetries are the documented design (`Router::physical_claims_by_request`'s
//! own comment, and `reactivate_covered_atom`'s "later metadata-only
//! attachments ... cannot grow this bounded sidecar"). `physical_agrees`
//! below marks, per case, whether the case's sequence left the two views in
//! step; `PHYSICAL_OWNERSHIP_HISTORY_IS_NOT_IN_THE_PLAN` names the divergence
//! for the ones where it did not.
//!
//! ## Out of scope entirely
//!
//! The thirteen `Router` fields `rebuild_active_indexes` never touches —
//! `rules`, `prev_plan`, `last_diag`, `next_token`,
//! `refused_coverage_assignments_by_demand`, `uncovered_by_demand`,
//! `uncovered_owners_by_author`, `refusals_by_demand`,
//! `refused_request_owner_counts`, `refused_owner_counts_by_session`,
//! `admission_work`, `full_metadata_work`, `withdrawal_work` — have no
//! from-scratch reconstructor at all, so nothing here can prove anything
//! about them. `plan_derived_census` zeroes exactly the census columns they
//! feed.

use std::collections::BTreeSet;

use nmp_grammar::{ConcreteFilter, ContextualAtom, ReadRouting};
use nostr::{Keys, RelayUrl, SecretKey};

use crate::facts::LocalFacts;
use crate::{PublicKey, Router, RouterOwnershipCensus, RuleRegistry};

fn author(index: u8) -> PublicKey {
    let mut bytes = [0u8; 32];
    bytes[0] = 1;
    bytes[31] = index + 1;
    Keys::new(SecretKey::from_slice(&bytes).unwrap()).public_key()
}

fn relay(url: &str) -> RelayUrl {
    RelayUrl::parse(url).unwrap()
}

fn pinned(relay: &RelayUrl, kinds: impl IntoIterator<Item = u16>) -> ContextualAtom {
    ContextualAtom {
        filter: ConcreteFilter {
            kinds: Some(kinds.into_iter().collect()),
            ..ConcreteFilter::default()
        },
        routing: ReadRouting::Explicit(vec![relay.clone()]),
        authenticate_as: None,
        routing_evidence: BTreeSet::new(),
    }
}

fn outbox(authors: impl IntoIterator<Item = PublicKey>, kind: u16) -> ContextualAtom {
    ContextualAtom {
        filter: ConcreteFilter {
            kinds: Some(BTreeSet::from([kind])),
            authors: Some(authors.into_iter().map(|author| author.to_hex()).collect()),
            ..ConcreteFilter::default()
        },
        routing: ReadRouting::Auto,
        authenticate_as: None,
        routing_evidence: BTreeSet::new(),
    }
}

/// Two authors with overlapping outbound sets, so one coverage solve puts
/// both on the same relay and the resulting requests carry real provenance,
/// coverage assignments and diagnostic author refs.
fn directory() -> LocalFacts {
    let shared = relay("wss://agree-outbox-shared.example");
    LocalFacts::new()
        .with_author_routes(
            author(0),
            [shared.clone(), relay("wss://agree-outbox-a.example")],
            [],
        )
        .with_author_routes(
            author(1),
            [shared, relay("wss://agree-outbox-b.example")],
            [],
        )
}

/// A router holding the same `(rules, prev_plan, active_demands)` whose
/// active indexes were built in one pass instead of incrementally.
fn rebuilt_twin(router: &Router) -> Router {
    let mut twin = Router::new(RuleRegistry::default_widen_only());
    twin.prev_plan = router.prev_plan.clone();
    twin.rebuild_active_indexes(router.active_demands.values().cloned());
    twin
}

/// The census columns `rebuild_active_indexes` reconstructs out of the plan
/// alone. Zeroes the fourteen columns fed by fields it never touches, and the
/// six fed by the four install-time physical indexes the plan cannot carry.
/// The five `plan_*` columns are left in: they are equal by construction and
/// catch a twin built from the wrong plan.
fn plan_derived_census(router: &Router) -> RouterOwnershipCensus {
    RouterOwnershipCensus {
        physical_request_claim_keys: 0,
        physical_claim_keys: 0,
        physical_claim_edges: 0,
        physical_request_contribution_keys: 0,
        physical_demand_keys: 0,
        physical_demand_edges: 0,
        ..full_census(router)
    }
}

/// Every census column a rebuild can reach, including the physical six.
fn full_census(router: &Router) -> RouterOwnershipCensus {
    RouterOwnershipCensus {
        refused_coverage_assignment_demands: 0,
        refused_coverage_assignment_authors: 0,
        refusal_demand_keys: 0,
        refusal_demand_edges: 0,
        refused_request_owner_keys: 0,
        refused_session_owner_keys: 0,
        uncovered_demand_keys: 0,
        uncovered_author_keys: 0,
        uncovered_author_refs: 0,
        diagnostic_sessions: 0,
        diagnostic_uncovered_authors: 0,
        diagnostic_sessions_refused_by_cap: 0,
        diagnostic_sessions_refused_by_subscription_budget: 0,
        diagnostic_dropped_merge_rules: 0,
        ..router.ownership_census()
    }
}

/// Assert the thirteen indexes a rebuild really can re-derive.
fn assert_plan_derived_indexes_agree(case: &str, router: &Router, twin: &Router) {
    assert_eq!(
        plan_derived_census(router),
        plan_derived_census(twin),
        "{case}: plan-derived census"
    );
    assert_eq!(
        router.active_demands, twin.active_demands,
        "{case}: active_demands"
    );
    assert_eq!(
        router.requests_by_demand, twin.requests_by_demand,
        "{case}: requests_by_demand"
    );
    assert_eq!(
        router.active_by_request, twin.active_by_request,
        "{case}: active_by_request"
    );
    assert_eq!(
        router.request_coverage_by_key, twin.request_coverage_by_key,
        "{case}: request_coverage_by_key"
    );
    assert_eq!(
        router.request_position_by_key, twin.request_position_by_key,
        "{case}: request_position_by_key"
    );
    assert_eq!(
        router.request_by_exact_filter, twin.request_by_exact_filter,
        "{case}: request_by_exact_filter"
    );
    assert_eq!(
        router.request_owner_contributions, twin.request_owner_contributions,
        "{case}: request_owner_contributions"
    );
    assert_eq!(
        router.request_claim_owner_counts, twin.request_claim_owner_counts,
        "{case}: request_claim_owner_counts"
    );
    assert_eq!(
        router.request_provenance_owner_counts, twin.request_provenance_owner_counts,
        "{case}: request_provenance_owner_counts"
    );
    assert_eq!(
        router.request_demand_coverage_owner_counts, twin.request_demand_coverage_owner_counts,
        "{case}: request_demand_coverage_owner_counts"
    );
    assert_eq!(
        router.coverage_assignment_requests, twin.coverage_assignment_requests,
        "{case}: coverage_assignment_requests"
    );
    assert_eq!(
        router.active_outbox_authors, twin.active_outbox_authors,
        "{case}: active_outbox_authors"
    );
    assert_eq!(
        router.diagnostic_author_refs, twin.diagnostic_author_refs,
        "{case}: diagnostic_author_refs"
    );
}

/// Assert the four install-time physical indexes as well. Sound only where
/// the case left `WireReq` ownership equal to install-time ownership.
fn assert_physical_indexes_agree(case: &str, router: &Router, twin: &Router) {
    assert_eq!(
        full_census(router),
        full_census(twin),
        "{case}: full census"
    );
    assert_eq!(
        router.physical_claims_by_request, twin.physical_claims_by_request,
        "{case}: physical_claims_by_request"
    );
    assert_eq!(
        router.requests_by_physical_claim, twin.requests_by_physical_claim,
        "{case}: requests_by_physical_claim"
    );
    assert_eq!(
        router.physical_contributions_by_request, twin.physical_contributions_by_request,
        "{case}: physical_contributions_by_request"
    );
    assert_eq!(
        router.requests_by_physical_demand, twin.requests_by_physical_demand,
        "{case}: requests_by_physical_demand"
    );
}

fn assert_non_empty(case: &str, router: &Router) {
    assert!(
        !router.active_demands.is_empty(),
        "{case}: the case must stop at a non-empty state, or it proves nothing"
    );
    assert!(
        !router.prev_plan.reqs.is_empty(),
        "{case}: the case must leave at least one running request"
    );
}

/// Admit three pinned atoms across two relays, then drop one of them. The
/// surviving state has both a closed request and live ones.
fn admit_then_withdraw_partial(router: &mut Router) {
    let first = relay("wss://agree-partial-first.example");
    let second = relay("wss://agree-partial-second.example");
    router.admit(
        &BTreeSet::from([
            pinned(&first, [1]),
            pinned(&first, [2]),
            pinned(&second, [3]),
        ]),
        &LocalFacts::new(),
        20,
    );
    let withdrawal = router.withdraw([pinned(&first, [1])], 20);
    assert!(
        withdrawal.diagnostics_changed || !withdrawal.request_metadata_removals.is_empty(),
        "the partial withdrawal must actually remove something"
    );
}

/// Two later demands attach to one already-running byte-covering request
/// rather than opening wire work of their own — the coalescing case.
fn two_demands_share_one_coalesced_request(router: &mut Router) {
    let url = relay("wss://agree-coalesced.example");
    router.admit(
        &BTreeSet::from([pinned(&url, [1, 2])]),
        &LocalFacts::new(),
        20,
    );
    let attached = router.admit(
        &BTreeSet::from([pinned(&url, [1]), pinned(&url, [2])]),
        &LocalFacts::new(),
        20,
    );
    assert!(
        attached.wire.ops.is_empty(),
        "both narrow demands must attach to the incumbent, not open new requests"
    );
    assert_eq!(attached.request_metadata_updates.len(), 1);
}

/// The same shape, then one of the sharing owners leaves. The physical
/// request stays open under the remaining owners.
fn withdrawal_leaves_request_alive_under_another_owner(router: &mut Router) {
    let url = relay("wss://agree-survivor.example");
    router.admit(
        &BTreeSet::from([pinned(&url, [1, 2])]),
        &LocalFacts::new(),
        20,
    );
    router.admit(
        &BTreeSet::from([pinned(&url, [1]), pinned(&url, [2])]),
        &LocalFacts::new(),
        20,
    );
    let withdrawal = router.withdraw([pinned(&url, [1])], 20);
    assert!(
        withdrawal.wire.ops.is_empty(),
        "the request must survive its departing owner"
    );
    assert_eq!(withdrawal.request_metadata_removals.len(), 1);
}

/// An open-before-close replacement: a full recompile changes one running
/// request's filter, so the same wire token continues under a new `SubId`.
fn replacement_open_before_close(router: &mut Router) {
    let url = relay("wss://agree-replacement.example");
    router.compile(&BTreeSet::from([pinned(&url, [1])]), &LocalFacts::new(), 20);
    let outcome = router.compile(
        &BTreeSet::from([pinned(&url, [1, 2])]),
        &LocalFacts::new(),
        20,
    );
    assert!(
        !outcome.replacements.is_empty(),
        "the widened filter must replace the running request, not sit beside it"
    );
}

/// Outbox routing: real provenance, real coverage assignments, real
/// diagnostic author refs, plus one active demand that owns no request at
/// all.
fn outbox_coverage_then_bare_activate(router: &mut Router) {
    let facts = directory();
    let outcome = router.compile(
        &BTreeSet::from([outbox([author(0)], 1), outbox([author(1)], 1)]),
        &facts,
        20,
    );
    assert!(
        !outcome.wire.ops.is_empty(),
        "the coverage solve must put something on the wire"
    );
    assert!(
        !router.coverage_assignment_requests.is_empty(),
        "outbox routing must record typed coverage assignments"
    );
    // An owner with no route facts at all: active, but covered by nothing.
    router.activate(outbox([author(2)], 9));
}

/// A withdrawn owner of a still-open request is reattached through the
/// physical-contribution sidecar, without recompiling.
fn reactivate_a_covered_owner(router: &mut Router) {
    let url = relay("wss://agree-reactivate.example");
    let one = pinned(&url, [1]);
    let two = pinned(&url, [2]);
    router.admit(
        &BTreeSet::from([one.clone(), two.clone()]),
        &LocalFacts::new(),
        20,
    );
    assert_eq!(
        router.prev_plan.reqs.values().map(Vec::len).sum::<usize>(),
        1,
        "the cohort must coalesce into one request owning both demands"
    );
    router.withdraw([one.clone()], 20);
    router.activate(one.clone());
    assert!(
        router.reactivate_covered_atom(&one).is_some(),
        "the departed owner must be reattachable from physical contributions"
    );
}

/// A pending-only admission layered on top of a full compile, so the two
/// index-maintenance paths are mixed in one router.
fn compile_then_admit_a_later_cohort(router: &mut Router) {
    let facts = directory();
    let pin = relay("wss://agree-mixed-pinned.example");
    router.compile(&BTreeSet::from([outbox([author(0)], 1)]), &facts, 20);
    let outcome = router.admit(&BTreeSet::from([pinned(&pin, [7])]), &facts, 20);
    assert!(
        !outcome.wire.ops.is_empty(),
        "the later cohort must open its own request"
    );
}

/// One sequence, plus whether its end state left `WireReq` ownership equal to
/// the install-time ownership the physical indexes remember.
struct Case {
    name: &'static str,
    drive: fn(&mut Router),
    physical_agrees: bool,
}

const CASES: [Case; 7] = [
    Case {
        name: "admit_then_withdraw_partial",
        drive: admit_then_withdraw_partial,
        // The withdrawal prunes the departing owner off `WireReq` while its
        // install-time physical edges survive until the request closes.
        physical_agrees: false,
    },
    Case {
        name: "two_demands_share_one_coalesced_request",
        drive: two_demands_share_one_coalesced_request,
        // The two attached owners are on `WireReq` but not in the sidecar.
        physical_agrees: false,
    },
    Case {
        name: "withdrawal_leaves_request_alive_under_another_owner",
        drive: withdrawal_leaves_request_alive_under_another_owner,
        // Attach then partial withdraw: both asymmetries at once.
        physical_agrees: false,
    },
    Case {
        name: "replacement_open_before_close",
        drive: replacement_open_before_close,
        physical_agrees: true,
    },
    Case {
        name: "outbox_coverage_then_bare_activate",
        drive: outbox_coverage_then_bare_activate,
        physical_agrees: true,
    },
    Case {
        // Withdraw then reattach restores exactly the install-time ownership,
        // so the two views come back into step.
        name: "reactivate_a_covered_owner",
        drive: reactivate_a_covered_owner,
        physical_agrees: true,
    },
    Case {
        name: "compile_then_admit_a_later_cohort",
        drive: compile_then_admit_a_later_cohort,
        physical_agrees: true,
    },
];

#[test]
fn incremental_indexes_agree_with_a_from_scratch_rebuild() {
    for case in CASES {
        let mut router = Router::new(RuleRegistry::default_widen_only());
        (case.drive)(&mut router);
        assert_non_empty(case.name, &router);
        let twin = rebuilt_twin(&router);
        assert_plan_derived_indexes_agree(case.name, &router, &twin);
        if case.physical_agrees {
            assert_physical_indexes_agree(case.name, &router, &twin);
        }
    }
}

/// The claim `rebuild_active_indexes` cannot support, kept runnable so the
/// divergence stays reproducible rather than becoming folklore.
///
/// MEASURED DIVERGENCE (2026-08-19, `origin/master` d9824def), all four
/// install-time physical indexes plus the six census columns they feed:
///
/// - `two_demands_share_one_coalesced_request` — incremental
///   `physical_contributions_by_request` holds ONE demand (the original wide
///   `kinds {1,2}` owner); the rebuild holds THREE (`{1}`, `{1,2}`, `{2}`),
///   because the two later owners were attached to `WireReq.owner_demands`
///   as detachable local metadata and never entered the sidecar.
/// - `admit_then_withdraw_partial` — incremental holds the withdrawn
///   `kinds {1}` owner's contribution and claim; the rebuild does not,
///   because withdrawal pruned them off `WireReq` while the physical edges
///   stay until the request physically closes.
/// - `withdrawal_leaves_request_alive_under_another_owner` — both at once:
///   incremental holds ONE demand, the rebuild holds TWO.
///
/// This is not drift in the incremental indexes. It is that the sidecar
/// records history the running plan does not carry, so
/// `(prev_plan.reqs, active_demands)` is not enough information to rebuild
/// it. Un-ignoring this test requires deciding whether the sidecar should
/// stop being install-time-only or the rebuild should stop claiming these
/// four fields — a design call, not a test fix.
#[test]
#[ignore = "physical ownership is install-time history the running plan does not carry; see the doc comment"]
fn install_time_physical_indexes_agree_with_a_from_scratch_rebuild() {
    for case in CASES {
        let mut router = Router::new(RuleRegistry::default_widen_only());
        (case.drive)(&mut router);
        assert_non_empty(case.name, &router);
        let twin = rebuilt_twin(&router);
        assert_physical_indexes_agree(case.name, &router, &twin);
    }
}
