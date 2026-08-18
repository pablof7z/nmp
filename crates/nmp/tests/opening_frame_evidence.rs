//! #1276: an observation's OPENING frame is its first frame — carrying the
//! opening rows, one acquisition-evidence entry per canonical branch, and the
//! execution facts issued while it was being assembled.
//!
//! Two separate lies are excluded, and both were reachable before this.
//!
//! **A frame reporting evidence for fewer branches than were declared.**
//! `RowBatch.evidence[i]` is documented on both SDKs as the fact about
//! `branches[i]` (`NMPDemand.kt`, `NMPLiveQuery.swift`), so a shorter vector
//! breaks an index correspondence rather than merely reporting less detail.
//! This is what `NMPLiveQueryTest.branchCountMatchesDeliveredEvidenceCount` and
//! its Swift twin were failing on in CI.
//!
//! **A frame reporting evidence for a projection whose rows have not been
//! delivered.** Evidence describes where the rows came from, so shipping a
//! proven source ahead of its rows is not "early", it is wrong: `nmp-nip02`
//! reads `availability` off `frame.evidence` and the contact list off the
//! deltas it has folded, and the mismatched pair makes it report
//! `NoContactList` for an account that has one.
//!
//! The window both live in is an interleaving, not a delay. `Cmd::Subscribe`
//! hands the `RowsReceiver` back to the caller BEFORE it dispatches the
//! observation's opening effects, and those effects carry each branch's
//! `ConcreteFilter { cause: Initial }` execution fact AHEAD of the opening
//! `Effect::EmitRows`. A caller that reaches `recv()` in between used to
//! consume an execution-only frame as its first delivery. Now those facts wait
//! for the opening frame and ride out on it, so there is no frame to get
//! either fact wrong. This test opens the observation repeatedly so both
//! dispatch interleavings are exercised, and checks EVERY frame the opening
//! produces rather than only the first.
//!
//! Disablements that must turn this red, in `RowsSender::send_evidence` — both
//! are ways of letting a frame precede the opening projection:
//!
//! - build it from an empty acquisition snapshot (the shipped defect): the
//!   first frame reports evidence for zero branches. Observed in ~5% of
//!   openings on an unloaded macOS host (56/1000), and the source of the CI
//!   flake.
//! - build it from the opening projection's evidence: the count is right and
//!   the rows are still missing, which `nmp-parity`'s
//!   `direct_and_ffi_follow_actions_are_identical_over_real_loopback` catches
//!   (18/20 runs) because the follow action reads both off one frame.
//!
//! The mechanism itself is pinned deterministically, with no interleaving, by
//! `nmp::runtime::row_channel::opening_execution_facts_ride_out_on_the_opening_frame`.

use std::collections::BTreeSet;
use std::time::Duration;

use nmp_grammar::{Demand, Filter, Freshness, LiveQuery, ReadRouting};
use nmp_router_testkit::FixtureRoutingFacts;
use nmp_runtime::EngineThread;
use nmp_store::RedbStore;
use nmp_transport::PoolConfig;
use nostr::RelayUrl;

/// One cache-only branch pinned to one relay. Cache-only keeps the whole test
/// off any socket: nothing here waits on a network, only on the engine
/// thread's own dispatch.
fn branch(host: &str) -> LiveQuery {
    let mut demand = Demand::new(
        Filter {
            kinds: Some(BTreeSet::from([1u16])),
            ..Filter::default()
        },
        ReadRouting::Explicit(vec![RelayUrl::parse(host).expect("fixture url")])
    )
    .expect("a one-relay pinned set is nonempty");
    demand.freshness = Freshness::CacheOnly;
    LiveQuery::single(demand)
}

/// How many openings to exercise. Each is a full engine spawn/shutdown; the
/// count exists to cover both dispatch interleavings, never to wait for one.
const OPENINGS: usize = 256;

#[test]
fn every_opening_frame_reports_one_evidence_entry_per_branch() {
    let mut openings_carrying_facts = 0usize;

    for opening in 0..OPENINGS {
        let (engine_thread, handle) = EngineThread::spawn_with_fixture_routing_facts(
            RedbStore::temporary().expect("temporary Redb store"),
            FixtureRoutingFacts::new(),
            10,
            PoolConfig {
                reconnect_delay_initial: Some(Duration::from_secs(3600)),
                ..PoolConfig::default()
            },
        )
        .expect("spawn runtime");

        let query = LiveQuery::union(
            [
                branch("wss://a.example.com"),
                branch("wss://b.example.com"),
                branch("wss://a.example.com"),
            ],
            None,
        )
        .expect("a three-declaration union canonicalizes to two branches");
        let branches = query.branches().len();
        assert_eq!(branches, 2, "the duplicate declaration collapses");

        let (_query, rows) = handle.subscribe(query).expect("subscribe");

        // `recv` terminates on the first delivered frame — a fact, not a
        // deadline. The timeout only keeps a genuine hang from wedging CI.
        let (_, evidence, execution) = rows
            .recv_timeout(Duration::from_secs(30))
            .expect("the opening always delivers a frame");
        assert_eq!(
            evidence.len(),
            branches,
            "opening {opening}: first frame carried {} evidence entries for {branches} \
             declared branches (execution facts: {})",
            evidence.len(),
            execution.len(),
        );
        if !execution.is_empty() {
            openings_carrying_facts += 1;
        }

        // Whatever else the opening already queued must agree too.
        while let Ok((_, evidence, _)) = rows.try_recv() {
            assert_eq!(
                evidence.len(),
                branches,
                "opening {opening}: a later opening frame carried {} evidence entries",
                evidence.len(),
            );
        }

        handle.shutdown();
        engine_thread.join();
    }

    // The facts issued before the opening frame are carried BY it. If holding
    // them ever turned into dropping them, this is the assertion that notices:
    // every opening resolves two pinned branches, so every first frame has
    // `ConcreteFilter` facts to carry.
    assert_eq!(
        openings_carrying_facts, OPENINGS,
        "the opening frame carries the execution facts issued while it was assembled"
    );
}
