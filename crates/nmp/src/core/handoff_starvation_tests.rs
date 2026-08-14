//! Falsifiers for the one lane state that could stop being a wait and start
//! being a hang (#1316).
//!
//! `schedule_ready` applies a ratified one-attempt-per-relay cap, and it
//! counts `PublishQueueLaneState::InFlight` regardless of phase. Every other
//! lane state carries its own exit — a durable deadline (`AwaitingAck`,
//! `Transient`) or a connection fact (`Eligible`, `WaitingConnection`,
//! `WaitingAuth`). `InFlight { AwaitingHandoff }` carries neither: its only
//! exit is the single `EngineMsg::EventHandoff` for its correlation. Consume
//! that one-shot and lose it, and the lane holds its relay's only attempt
//! slot for the life of the process — which is what wedged a production
//! publish lane behind one entry while the backlog grew monotonically.

use nmp_store::{EventStore, RedbStore};
use nostr::{Keys, Kind, RelayUrl, Timestamp};

use super::*;

// ---- fixtures ----------------------------------------------------------

fn session_for(relay: &RelayUrl, author: &Keys) -> RelaySessionKey {
    RelaySessionKey::new(relay.clone(), AccessContext::Nip42(author.public_key()))
}

/// Accept and sign one durable narrow write, returning the effects the
/// signer completion produced.
fn publish_narrow(
    core: &mut EngineCore,
    author: &Keys,
    relay: &RelayUrl,
    created_at: u64,
) -> (ReceiptId, SignedEvent, Vec<Effect>) {
    core.handle(EngineMsg::SetActivePubkey(Some(author.public_key())));
    let accepted = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(nmp_grammar::EventBuilder {
            kind: Kind::TextNote,
            tags: (Vec::new()).into_iter().collect(),
            content: format!("handoff starvation {created_at}"),
            created_at: Some(Timestamp::from(created_at)),
        }),
        routing: WriteRouting::Explicit(Vec::from([relay.clone()])),
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

fn publish_correlation(
    effects: &[Effect],
    session: &RelaySessionKey,
) -> Option<AttemptCorrelation> {
    effects.iter().find_map(|effect| match effect {
        Effect::PublishEvent(candidate, _, correlation) if candidate == session => {
            Some(*correlation)
        }
        _ => None,
    })
}

fn lane_state(core: &EngineCore, receipt: ReceiptId) -> PublishQueueLaneState {
    let intent_id = core.pending[&receipt].intent_id;
    core.store
        .recover_publish_queue_lanes(intent_id)
        .expect("lane recovery")
        .into_iter()
        .next()
        .expect("the write owns exactly one lane")
        .state
}

// ---- falsifiers --------------------------------------------------------

/// The production wedge, reproduced.
///
/// One write's handoff result is consumed by the reducer and then lost to a
/// store refusal that heals on the very next call. The transport will not
/// repeat it (#93: one `HandoffResult` per correlation, ever), so nothing in
/// the process can deliver another. On the unfixed reducer that lane holds
/// the relay's only attempt slot forever and EVERY later write to the same
/// relay sits `Eligible` and never dispatches — reads on the relay keep
/// working, so the failure looks like a stalled publisher rather than a
/// broken relay.
#[test]
fn a_handoff_that_can_never_arrive_must_not_starve_its_relay() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://handoff-starvation.example.com").unwrap();
    let session = session_for(&relay, &author);
    let handle = TransportRelayHandle {
        slot: 3,
        generation: 1,
    };
    let mut core = EngineCore::new(
        RedbStore::temporary_with_failed_lane_handoff()
            .expect("temporary Redb handoff-failure fixture"),
        10,
    );

    core.handle(EngineMsg::RelayConnected(handle, session.clone()));
    let (receipt_a, _, _) = publish_narrow(&mut core, &author, &relay, 900);
    let released = core.handle(EngineMsg::AuthProbeReleased(handle, session.clone()));
    let correlation = publish_correlation(&released, &session)
        .expect("the first eligible lane starts one attempt");

    // The one-shot result arrives and real Redb refuses exactly this
    // transition before commit. The construction arm is consumed immediately
    // -- the store is fine from the next call on -- but the correlation is
    // already gone.
    core.handle(EngineMsg::EventHandoff(correlation, HandoffResult::Written));

    assert!(
        matches!(
            lane_state(&core, receipt_a),
            PublishQueueLaneState::InFlight {
                phase: PublishQueueInFlightPhase::AwaitingHandoff,
                ..
            }
        ),
        "the refused commit must leave the lane exactly as durable as it was"
    );
    assert!(
        core.attempt_correlations.is_empty(),
        "the one-shot handoff was consumed, so nothing can deliver another"
    );

    // A later write to the same relay. This is the one that must not starve.
    let (receipt_b, _, signed_b_effects) = publish_narrow(&mut core, &author, &relay, 901);
    let mut dispatched_b = publish_correlation(&signed_b_effects, &session).is_some();

    // Drive every fact the process can still produce without a restart: the
    // deadline sweep, plain ticks, and a full disconnect/reconnect cycle --
    // a connection fact is what rescues `WaitingAuth` and `AwaitingAck`, so
    // this proves it is not what rescues `AwaitingHandoff`.
    for step in 0..6u64 {
        let now = Timestamp::from(1_000 + step * 600);
        dispatched_b |= publish_correlation(&core.tick(now), &session).is_some();
        if let Some(due) = core.next_deadline().expect("deadline peek") {
            dispatched_b |= publish_correlation(&core.tick(due), &session).is_some();
        }
        let reconnected = TransportRelayHandle {
            slot: 3,
            generation: 2 + step,
        };
        core.handle(EngineMsg::RelayDisconnected(
            handle,
            session.clone(),
            DisconnectReason::Error,
        ));
        core.handle(EngineMsg::RelayConnected(reconnected, session.clone()));
        let woken = core.handle(EngineMsg::AuthProbeReleased(reconnected, session.clone()));
        dispatched_b |= publish_correlation(&woken, &session).is_some();
        if dispatched_b {
            break;
        }
    }

    assert!(
        dispatched_b,
        "a later write must not be starved behind a lane whose handoff can \
         never arrive; lane A = {:?}, lane B = {:?}",
        lane_state(&core, receipt_a),
        lane_state(&core, receipt_b),
    );
    assert!(
        !matches!(
            lane_state(&core, receipt_a),
            PublishQueueLaneState::InFlight {
                phase: PublishQueueInFlightPhase::AwaitingHandoff,
                ..
            }
        ),
        "the stuck lane must itself leave AwaitingHandoff, not merely stop \
         blocking its neighbours"
    );
}

/// The other half, and the one a careless fix breaks: a lane with a genuinely
/// outstanding correlation is WAITING, not hanging. Per-relay ordering is the
/// ratified cap, so a normal in-flight attempt must keep starving its
/// neighbours until its own handoff resolves.
#[test]
fn a_live_in_flight_attempt_still_holds_its_relays_only_slot() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://handoff-live.example.com").unwrap();
    let session = session_for(&relay, &author);
    let handle = TransportRelayHandle {
        slot: 4,
        generation: 1,
    };
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 10);

    core.handle(EngineMsg::RelayConnected(handle, session.clone()));
    let (receipt_a, signed_a, _) = publish_narrow(&mut core, &author, &relay, 910);
    let released = core.handle(EngineMsg::AuthProbeReleased(handle, session.clone()));
    let correlation =
        publish_correlation(&released, &session).expect("the first lane starts one attempt");

    let (_, _, signed_b_effects) = publish_narrow(&mut core, &author, &relay, 911);
    assert!(
        publish_correlation(&signed_b_effects, &session).is_none(),
        "the per-relay cap must hold while an attempt is genuinely in flight"
    );
    for step in 0..3u64 {
        let effects = core.tick(Timestamp::from(2_000 + step * 60));
        assert!(
            publish_correlation(&effects, &session).is_none(),
            "a tick must not break the per-relay cap for a live attempt"
        );
    }
    assert!(
        matches!(
            lane_state(&core, receipt_a),
            PublishQueueLaneState::InFlight {
                phase: PublishQueueInFlightPhase::AwaitingHandoff,
                ..
            }
        ),
        "an outstanding correlation is a wait, and must be left alone"
    );

    // Resolving it hands the lane on to its ACK deadline -- the fix must not
    // have stolen a live attempt -- and finishing the attempt releases the
    // relay's slot to the queued write exactly as before.
    core.handle(EngineMsg::EventHandoff(correlation, HandoffResult::Written));
    assert!(
        matches!(
            lane_state(&core, receipt_a),
            PublishQueueLaneState::InFlight {
                phase: PublishQueueInFlightPhase::AwaitingAck { .. },
                ..
            }
        ),
        "a resolved handoff advances the same attempt, it does not restart it"
    );
    let acked = core.handle(EngineMsg::RelayFrame(
        handle,
        session.clone(),
        RelayFrame::from(nostr::RelayMessage::ok(signed_a.id, true, "")),
    ));
    assert!(
        publish_correlation(&acked, &session).is_some(),
        "the queued write proceeds once the slot is genuinely free"
    );
}

/// The restart case, which used to have its own arm in `open_bootstrapped_lanes`
/// and is now a consequence of the general rule: a fresh process holds no
/// correlations at all, so every recovered `AwaitingHandoff` lane is orphaned
/// by definition. Deleting the arm must not cost the behaviour it carried.
#[test]
fn a_lane_recovered_into_awaiting_handoff_is_reclaimed_by_the_general_rule() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://handoff-restart.example.com").unwrap();
    let session = session_for(&relay, &author);
    let handle = TransportRelayHandle {
        slot: 5,
        generation: 1,
    };
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("handoff-restart.redb");

    let receipt_a = {
        let mut core = EngineCore::new(RedbStore::open(&path).unwrap(), 10);
        core.handle(EngineMsg::RelayConnected(handle, session.clone()));
        let (receipt_a, _, _) = publish_narrow(&mut core, &author, &relay, 920);
        let released = core.handle(EngineMsg::AuthProbeReleased(handle, session.clone()));
        assert!(
            publish_correlation(&released, &session).is_some(),
            "the lane must be in flight when the process dies"
        );
        assert!(matches!(
            lane_state(&core, receipt_a),
            PublishQueueLaneState::InFlight {
                phase: PublishQueueInFlightPhase::AwaitingHandoff,
                ..
            }
        ));
        receipt_a
    };

    // A new process. Nothing carried the correlation across, which IS the
    // fact that says the handoff can never arrive.
    let mut rebooted = EngineCore::new(RedbStore::open(&path).unwrap(), 10);
    let recovered = rebooted.recover_on_boot();
    // Reclaiming BEFORE this boot's own deadline sweep is what lets the same
    // boot finish the job: the replacement attempt is eligible as of `now`,
    // the sweep promotes it, and the ordinary eligible path asks for its
    // session -- no second tick required.
    assert_eq!(
        lane_state(&rebooted, receipt_a),
        PublishQueueLaneState::Eligible {
            since: rebooted.clock
        },
        "a recovered in-flight handoff must not survive the boot that cannot answer it"
    );
    assert!(
        recovered.iter().any(|effect| matches!(
            effect,
            Effect::EnsureWriteRelay(candidate) if candidate == &session
        )),
        "the reclaimed lane's session must be asked for, got {recovered:?}"
    );

    // And it really does publish again, under a fresh attempt.
    rebooted.handle(EngineMsg::RelayConnected(handle, session.clone()));
    let woken = rebooted.handle(EngineMsg::AuthProbeReleased(handle, session.clone()));
    assert!(
        publish_correlation(&woken, &session).is_some(),
        "the recovered write must reach the wire again, got {woken:?}"
    );
}
