//! The freshness falsifier `StalledWriteCensus::assert_consistent` exists
//! for (#1743).
//!
//! This owner holds three artifacts that are supposed to move together on
//! every real change: the private change-detector `census`, the projected
//! `rows`, and their `totals`. `rows`/`totals` are the only ones any
//! existing behavioral test can see, because `diagnostics_snapshot` reads
//! them directly and `project()` always recomputes both from `pending`
//! independently of `census`. So a bug that corrupts `census` alone --
//! records the wrong stage, or misses an entry -- produces a CORRECT
//! snapshot on the very turn it happens, and only misleads a LATER
//! incremental `refresh` that binary-searches the wrong baseline. No
//! behavioral assertion over `diagnostics_snapshot` can see that census
//! is wrong; only comparing it to a fresh recompute can.
//!
//! `assert_owner_consistency` does that: `StalledWriteCensus::assert_consistent`
//! recomputes `census`/`rows`/`totals` from scratch and compares each
//! independently.

use super::*;
use nmp_router_testkit::FixtureRoutingFacts;

/// One write, parked `Undeliverable` on an unreachable destination, then
/// cleared by connecting it. Both transitions are asserted against the
/// mirror BEFORE and AFTER, so a run against the intact owner establishes
/// the precondition this falsifier needs: the write really does become
/// stalled, and the transition that clears it really is observed through
/// the ordinary effect-driven refresh path, not skipped because nothing
/// ever changed.
#[test]
fn stalled_write_census_agrees_with_a_fresh_recompute_across_parking_and_clearing() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://stalled-write-freshness.example").unwrap();
    let directory =
        FixtureRoutingFacts::new().with_outbound_routes(author.public_key(), [relay.clone()]);
    let mut core = EngineCore::new_with_fixture_routing_facts(
        RedbStore::temporary().expect("temporary Redb store"),
        directory,
        10,
    );
    core.handle(EngineMsg::SetActivePubkey(Some(author.public_key())));
    core.assert_owner_consistency("empty");

    let accepted = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(nmp_grammar::EventBuilder {
            kind: Kind::TextNote,
            tags: (vec![]).into_iter().collect(),
            content: ("freshness falsifier").into(),
            created_at: Some(Timestamp::from(1u64)),
        }),
        routing: WriteRouting::Explicit(vec![relay.clone()]),
        identity: Identity::Active,
        correlation: None,
    }));
    let (receipt, generation, unsigned) = accepted
        .iter()
        .find_map(|effect| match effect {
            Effect::RequestSign(receipt, generation, unsigned) => {
                Some((*receipt, *generation, unsigned.clone()))
            }
            _ => None,
        })
        .expect("accepted unsigned intent requests signing");
    core.handle(EngineMsg::SignerCompleted(
        receipt,
        generation,
        Ok(unsigned.sign_with_keys(&author).unwrap()),
    ));

    // Precondition, asserted BEFORE the mirror check this test exists to
    // exercise: the write really did become stalled just now, through the
    // ordinary acceptance path. If setup silently produced zero stalled
    // writes, a passing `assert_owner_consistency` below would prove
    // nothing -- the census would trivially agree with a fresh recompute
    // of an empty population, indistinguishable from the guard actually
    // holding.
    assert_eq!(
        core.diagnostics_snapshot().stalled_writes.len(),
        1,
        "setup must produce exactly one stalled write before the mirror check"
    );
    core.assert_owner_consistency("after parking");

    // Connect the write's only destination. This is the transition that
    // exercises the `(Ok(index), Some(stage)) => ..` and removal arms of
    // `StalledWriteCensus::refresh` -- an existing census entry either
    // changes stage or is removed, not merely inserted fresh.
    let session = RelaySessionKey::new(relay.clone(), AccessContext::Nip42(author.public_key()));
    core.handle(EngineMsg::RelayConnected(
        TransportRelayHandle {
            slot: 0,
            generation: 1,
        },
        session,
    ));

    assert_eq!(
        core.diagnostics_snapshot().stalled_writes.len(),
        0,
        "connecting the write's only destination must clear the stall"
    );
    core.assert_owner_consistency("after connecting");
}
