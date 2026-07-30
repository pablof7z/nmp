//! Falsifiers for the way OUT of conservative lane retention (#1000).
//!
//! #988 made a failed `bootstrap_outbox_lanes` retain every route candidate
//! as `uncertain`, which is right, and then gave that retention no exit:
//! `uncertain` is cleared only by a committed `RecoveredLane` for that exact
//! relay, and an intent whose bootstrap failed owns no lane rows for any
//! other path to commit. These tests pin both halves — retention while the
//! projection is genuinely unknown, AND release once the store answers.

use std::collections::BTreeSet;

use nmp_store::{EventStore, MemoryStore, PersistenceFault, RedbStore};
use nostr::{Keys, Kind, RelayMessage, RelayUrl, Timestamp};

use crate::lane_fault_store::{FaultyLaneStore, LaneFaults};

use super::*;

// ---- fixtures ----------------------------------------------------------

fn session_for(relay: &RelayUrl, author: &Keys) -> RelaySessionKey {
    RelaySessionKey::new(relay.clone(), AccessContext::Nip42(author.public_key()))
}

/// Accept and sign one durable narrow write, returning the effects the
/// signer completion produced.
fn publish_narrow<S: EventStore>(
    core: &mut EngineCore<S>,
    author: &Keys,
    relays: &[RelayUrl],
    created_at: u64,
) -> (ReceiptId, SignedEvent, Vec<Effect>) {
    core.handle(EngineMsg::SetActivePubkey(Some(author.public_key())));
    let accepted = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(nmp_grammar::EventBuilder {
            kind: Kind::TextNote,
            tags: (Vec::new()).into_iter().collect(),
            content: format!("bootstrap retry {created_at}"),
            created_at: Some(Timestamp::from(created_at)),
        }),
        durability: Durability::Durable,
        routing: WriteRouting::Explicit(Vec::from_iter(relays.to_vec())),
        identity: Identity::Active,
        correlation: None,
    }));
    let (id, generation, unsigned) = accepted
        .iter()
        .find_map(|effect| match effect {
            Effect::RequestSign(id, generation, unsigned) => {
                Some((*id, *generation, unsigned.clone()))
            }
            _ => None,
        })
        .expect("accepted write requests signing");
    let signed = unsigned.sign_with_keys(author).expect("sign fixture");
    let effects = core.handle(EngineMsg::SignerCompleted(
        id,
        generation,
        Ok(signed.clone()),
    ));
    (id, signed, effects)
}

/// Drive one relay all the way to a durable OK ack.
fn deliver_ok<S: EventStore>(
    core: &mut EngineCore<S>,
    author: &Keys,
    relay: &RelayUrl,
    slot: u32,
    signed: &SignedEvent,
) {
    let session = session_for(relay, author);
    let handle = TransportRelayHandle {
        slot,
        generation: 1,
    };
    core.handle(EngineMsg::RelayConnected(handle, session.clone()));
    let scheduled = core.handle(EngineMsg::AuthProbeReleased(handle, session.clone()));
    let correlation = scheduled
        .iter()
        .find_map(|effect| match effect {
            Effect::PublishEvent(candidate, _, correlation) if candidate == &session => {
                Some(*correlation)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("eligible lane on {relay} starts one attempt"));
    core.handle(EngineMsg::EventHandoff(correlation, HandoffResult::Written));
    core.handle(EngineMsg::RelayFrame(
        handle,
        session,
        RelayFrame::from(RelayMessage::ok(signed.id, true, "")),
    ));
}

/// The canonical answer, rebuilt from durable rows rather than read out of
/// the projection under test.
fn durable_worker_oracle<S: EventStore>(core: &EngineCore<S>) -> BTreeSet<RelaySessionKey> {
    let mut expected: BTreeSet<_> = core
        .attempt_correlations
        .values()
        .map(|target| target.session.clone())
        .collect();
    for pending in core.pending.values() {
        let access = AccessContext::Nip42(pending.signing_pubkey);
        expected.extend(
            pending
                .pending_relays
                .iter()
                .chain(&pending.unstarted_relays)
                .chain(&pending.route_blocked_relays)
                .cloned()
                .map(|relay| RelaySessionKey::new(relay, access)),
        );
        if let Some(intent_id) = pending.intent_id {
            expected.extend(
                core.resolver
                    .store()
                    .recover_outbox_lanes(intent_id)
                    .expect("oracle lane recovery")
                    .into_iter()
                    .filter(|lane| !matches!(lane.state, LaneState::Terminal { .. }))
                    .map(|lane| RelaySessionKey::new(lane.key.relay, access)),
            );
        }
    }
    expected
}

// ---- falsifiers --------------------------------------------------------

/// The headline #1000 regression, once per durability classification.
///
/// A bootstrap that fails and then recovers must cost nothing permanent: the
/// worker set has to come back to exactly what a canonical rebuild from
/// durable rows yields (here, empty — the intent finishes and closes), and
/// the receipt has to leave `pending`. On the unfixed reducer the intent owns
/// no lane rows, so no committed lane fact can ever clear `uncertain`: both
/// relays stay pinned and the receipt never terminates.
fn transient_bootstrap_failure_is_fully_reversible(fault: PersistenceFault, seq: u64) {
    let author = Keys::generate();
    let relay_a = RelayUrl::parse("wss://bootstrap-retry-a.example.com").unwrap();
    let relay_b = RelayUrl::parse("wss://bootstrap-retry-b.example.com").unwrap();
    let faults = LaneFaults::default();
    faults.fail_bootstrap(fault);
    let mut core = EngineCore::new(FaultyLaneStore::new(MemoryStore::new(), faults.clone()), 10);

    let (receipt, signed, blocked) =
        publish_narrow(&mut core, &author, &[relay_a.clone(), relay_b.clone()], seq);
    assert!(
        blocked.iter().any(|effect| matches!(
            effect,
            Effect::EmitReceipt(id, WriteStatus::PersistenceBlocked(_)) if *id == receipt
        )),
        "the injected bootstrap failure must surface as PersistenceBlocked, got {blocked:?}"
    );

    // Retention first: while the projection is genuinely unknown BOTH route
    // candidates stay owned. A "fix" that dropped to under-retention here is
    // a hang, not an improvement.
    let pinned = core
        .relay_worker_requirements()
        .expect("known candidates keep the projection available")
        .writes;
    assert_eq!(
        pinned,
        BTreeSet::from([
            session_for(&relay_a, &author),
            session_for(&relay_b, &author)
        ]),
        "an unresolved bootstrap must retain every route candidate"
    );
    assert!(core.pending.contains_key(&receipt));

    // The store becomes usable again. Nothing external tells the reducer;
    // its own deadline is what brings it back.
    faults.heal();
    let due = core
        .next_deadline()
        .expect("an outstanding bootstrap gap arms a deadline");
    let retried = core.tick(due);
    assert!(
        retried.iter().any(|effect| matches!(
            effect,
            Effect::EnsureWriteRelay(session) if session == &session_for(&relay_a, &author)
        )),
        "the retried bootstrap must ask for its lanes' sessions, got {retried:?}"
    );
    assert!(
        core.pending[&receipt].lane_projection.uncertain.is_empty(),
        "a committed bootstrap replaces every conservative guess it stood in for"
    );

    deliver_ok(&mut core, &author, &relay_a, 0, &signed);
    deliver_ok(&mut core, &author, &relay_b, 1, &signed);

    assert!(
        !core.pending.contains_key(&receipt),
        "the receipt must reach a terminal state rather than parking in pending"
    );
    let after = core
        .relay_worker_requirements()
        .expect("the projection is available again")
        .writes;
    assert_eq!(after, durable_worker_oracle(&core));
    assert!(
        after.is_empty(),
        "a transient bootstrap failure must leave zero pinned workers, got {after:?}"
    );
}

#[test]
fn transient_io_bootstrap_failure_leaves_no_pinned_worker_behind() {
    transient_bootstrap_failure_is_fully_reversible(PersistenceFault::Io, 700);
}

#[test]
fn transient_invariant_bootstrap_failure_leaves_no_pinned_worker_behind() {
    transient_bootstrap_failure_is_fully_reversible(PersistenceFault::Invariant, 701);
}

/// An intent whose bootstrap failed must PROGRESS or TERMINATE. It may not
/// sit in `pending` with a non-empty `uncertain` set that nothing can drain:
/// `can_close` requires `uncertain` to be empty, so such an intent is
/// structurally stuck rather than merely waiting.
#[test]
fn a_failed_bootstrap_never_parks_an_intent_permanently() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://bootstrap-parked.example.com").unwrap();
    let faults = LaneFaults::default();
    faults.fail_bootstrap(PersistenceFault::Io);
    let mut core = EngineCore::new(FaultyLaneStore::new(MemoryStore::new(), faults.clone()), 10);

    let (receipt, signed, _) =
        publish_narrow(&mut core, &author, std::slice::from_ref(&relay), 702);
    assert!(
        !core.pending[&receipt].lane_projection.can_close(),
        "the gap really does block closure while it stands"
    );

    faults.heal();
    core.tick(core.next_deadline().expect("the gap arms a deadline"));
    deliver_ok(&mut core, &author, &relay, 0, &signed);

    assert!(
        !core.pending.contains_key(&receipt),
        "an intent whose bootstrap failed must still reach a terminal state"
    );
    assert!(
        core.lane_bootstrap_retries.is_empty(),
        "a committed bootstrap closes its own gap"
    );
}

/// Retention is not the bug and must survive the fix: while the store keeps
/// refusing, every route candidate stays owned across repeated ticks, and the
/// retry backs off rather than spinning.
#[test]
fn an_unresolved_bootstrap_keeps_retaining_and_backs_off() {
    let author = Keys::generate();
    let relay_a = RelayUrl::parse("wss://bootstrap-retain-a.example.com").unwrap();
    let relay_b = RelayUrl::parse("wss://bootstrap-retain-b.example.com").unwrap();
    let faults = LaneFaults::default();
    faults.fail_bootstrap(PersistenceFault::Io);
    let mut core = EngineCore::new(FaultyLaneStore::new(MemoryStore::new(), faults.clone()), 10);

    let (receipt, _, _) =
        publish_narrow(&mut core, &author, &[relay_a.clone(), relay_b.clone()], 703);
    let expected = BTreeSet::from([
        session_for(&relay_a, &author),
        session_for(&relay_b, &author),
    ]);

    let mut previous = Timestamp::from(0u64);
    for _ in 0..4 {
        let due = core
            .next_deadline()
            .expect("the gap keeps arming a deadline");
        assert!(due > previous, "the retry must back off, not spin");
        previous = due;
        core.tick(due);
        assert_eq!(
            core.relay_worker_requirements()
                .expect("known candidates keep the projection available")
                .writes,
            expected,
            "an unresolved bootstrap must keep retaining every candidate"
        );
        assert!(core.pending.contains_key(&receipt));
    }
    assert!(
        faults.bootstrap_calls() >= 5,
        "each due tick must actually re-attempt the bootstrap"
    );
}

/// A `recover_route_revisions` read error at boot cannot be allowed to
/// disable exact worker reconciliation for the rest of the process: with
/// `relay_worker_requirements` stuck at `None`, `retry_required_relay_workers`
/// returns early forever and a cap-refused write is never retried.
#[test]
fn a_boot_route_revision_read_error_re_enables_worker_reconciliation() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://bootstrap-boot-revisions.example.com").unwrap();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("bootstrap-retry.redb");

    {
        let mut core = EngineCore::new(RedbStore::open(&path).unwrap(), 10);
        publish_narrow(&mut core, &author, std::slice::from_ref(&relay), 704);
    }

    let faults = LaneFaults::default();
    faults.fail_route_revisions();
    let mut recovered = EngineCore::new(
        FaultyLaneStore::new(RedbStore::open(&path).unwrap(), faults.clone()),
        10,
    );
    recovered.recover_on_boot();
    assert!(
        recovered.relay_worker_requirements().is_none(),
        "an unreadable route set has nothing to hold as uncertain, so the \
         runtime must retain everything"
    );

    faults.heal();
    let due = recovered
        .next_deadline()
        .expect("the blind boot gap arms a deadline");
    recovered.tick(due);

    let requirements = recovered
        .relay_worker_requirements()
        .expect("a committed bootstrap must re-enable worker reconciliation");
    assert_eq!(requirements.writes, durable_worker_oracle(&recovered));
    assert_eq!(
        requirements.writes,
        BTreeSet::from([session_for(&relay, &author)]),
        "the recovered projection must name the durable lane's session"
    );

    // The same answer an untainted boot would have produced. The redb handle
    // is exclusive, so the tainted core has to release it first.
    let writes = requirements.writes.clone();
    drop(recovered);
    let mut fresh = EngineCore::new(RedbStore::open(&path).unwrap(), 10);
    fresh.recover_on_boot();
    assert_eq!(
        writes,
        fresh
            .relay_worker_requirements()
            .expect("a clean boot projects")
            .writes,
    );
}
