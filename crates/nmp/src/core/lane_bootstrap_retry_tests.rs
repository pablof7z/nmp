//! Falsifiers for the way OUT of conservative lane retention (#1000).
//!
//! #988 made a failed `bootstrap_publish_queue_lanes` retain every route candidate
//! as `uncertain`, which is right, and then gave that retention no exit:
//! `uncertain` is cleared only by a committed `PublishQueueLane` for that exact
//! relay, and an intent whose bootstrap failed owns no lane rows for any
//! other path to commit. These tests pin both halves — retention while the
//! projection is genuinely unknown, AND release once the store answers.

use std::borrow::Cow;
use std::collections::BTreeSet;

use nmp_store::{testing, EventStore, RedbStore};
use nostr::{Keys, Kind, RelayMessage, RelayUrl, Timestamp};

use super::*;

#[path = "lane_bootstrap_retry_tests/clock.rs"]
mod clock;

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
        expected.extend(
            core.resolver
                .store()
                .recover_publish_queue_lanes(pending.intent_id)
                .expect("oracle lane recovery")
                .into_iter()
                .filter(|lane| !matches!(lane.state, PublishQueueLaneState::Terminal { .. }))
                .map(|lane| RelaySessionKey::new(lane.key.relay, access)),
        );
    }
    expected
}

// ---- falsifiers --------------------------------------------------------

/// The headline #1000 regression through the real Redb transaction door.
///
/// A bootstrap that fails and then recovers must cost nothing permanent: the
/// worker set has to come back to exactly what a canonical rebuild from
/// durable rows yields (here, empty — the intent finishes and closes), and
/// the receipt has to leave `pending`. On the unfixed reducer the intent owns
/// no lane rows, so no committed lane fact can ever clear `uncertain`: both
/// relays stay pinned and the receipt never terminates.
#[test]
fn transient_redb_bootstrap_failure_is_fully_reversible() {
    let author = Keys::generate();
    let relay_a = RelayUrl::parse("wss://bootstrap-retry-a.example.com").unwrap();
    let relay_b = RelayUrl::parse("wss://bootstrap-retry-b.example.com").unwrap();
    let mut core = EngineCore::new(
        RedbStore::temporary_with_failed_lane_bootstrap()
            .expect("temporary Redb lane-bootstrap failure fixture"),
        10,
    );

    let (receipt, signed, blocked) =
        publish_narrow(&mut core, &author, &[relay_a.clone(), relay_b.clone()], 700);
    assert!(
        blocked.iter().any(|effect| matches!(
            effect,
            Effect::EmitReceipt(
                id,
                WriteFact::Relay {
                    state: RelayState::Waiting(RelayWaiting::PersistenceStalled { .. }),
                    ..
                }
            ) if *id == receipt
        )),
        "the Redb bootstrap refusal must surface as a persistence stall, got {blocked:?}"
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

    // The construction-armed refusal is consumed by the failed Redb
    // transaction. Nothing external tells the reducer; its own deadline is
    // what brings it back.
    let due = core
        .next_deadline()
        .expect("deadline peek")
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
    assert!(
        core.lane_bootstrap_retries.is_empty(),
        "a committed bootstrap closes its own retry gap"
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
fn oversized_auth_denial_reason_emits_no_terminal_receipt_fact() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://auth-denial-commit.example.com").unwrap();
    let session = session_for(&relay, &author);
    let handle = TransportRelayHandle {
        slot: 8,
        generation: 1,
    };
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 10);
    core.handle(EngineMsg::RelayConnected(handle, session.clone()));
    let (receipt, _, parked) =
        publish_narrow(&mut core, &author, std::slice::from_ref(&relay), 705);
    assert!(parked.iter().any(|effect| matches!(
        effect,
        Effect::EmitReceipt(id, WriteFact::Relay { state: RelayState::Waiting(RelayWaiting::NeedsAuth), .. }) if *id == receipt
    )));

    let challenged = core.handle(EngineMsg::RelayFrame(
        handle,
        session,
        RelayFrame::from(RelayMessage::Auth {
            challenge: Cow::Borrowed("commit-before-emit"),
        }),
    ));
    let token = challenged
        .into_iter()
        .find_map(|effect| match effect {
            Effect::RelayAuth(AuthEffect::RequestPolicy { token, .. }) => Some(token),
            _ => None,
        })
        .expect("challenge requests policy");
    let instance = AuthCapabilityInstance(705);
    core.handle(EngineMsg::AuthCapabilityBound {
        token: token.clone(),
        capability: AuthCapability::Policy,
        instance,
    });
    let denied = core.handle(EngineMsg::AuthPolicyCompleted(
        token,
        Some(instance),
        AuthPolicyOutcome::Deny {
            reason: "x".repeat(4_097),
        },
    ));
    assert!(
        !denied.iter().any(|effect| matches!(
            effect,
            Effect::EmitReceipt(id, WriteFact::Relay { state: RelayState::AuthFailed { .. }, .. }) if *id == receipt
        )),
        "a terminal receipt fact must not precede its refused durable transition: {denied:?}"
    );
    let intent = core.pending[&receipt].intent_id;
    assert_eq!(
        core.resolver
            .store()
            .recover_publish_queue_lanes(intent)
            .unwrap()[0]
            .state,
        PublishQueueLaneState::WaitingAuth
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
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("bootstrap-retain.redb");
    let (receipt, intent_id) = {
        let mut core = EngineCore::new(RedbStore::open(&path).unwrap(), 10);
        let (receipt, signed, _) =
            publish_narrow(&mut core, &author, &[relay_a.clone(), relay_b.clone()], 703);
        deliver_ok(&mut core, &author, &relay_a, 0, &signed);
        assert!(core.pending.contains_key(&receipt));
        (receipt, core.pending[&receipt].intent_id)
    };
    testing::corrupt_first_publish_queue_attempt(&path, &intent_id.0.to_be_bytes())
        .expect("store-owned persistent attempt corruption");

    let mut core = EngineCore::new(RedbStore::open(&path).unwrap(), 10);
    core.recover_on_boot();
    let expected = BTreeSet::from([
        session_for(&relay_a, &author),
        session_for(&relay_b, &author),
    ]);
    assert_eq!(core.lane_bootstrap_retries[&receipt].failures, 1);

    let mut previous = Timestamp::from(0u64);
    for expected_failures in 2..=5 {
        let due = core
            .next_deadline()
            .expect("deadline peek")
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
        assert_eq!(
            core.lane_bootstrap_retries[&receipt].failures, expected_failures,
            "each due tick must actually re-attempt the bootstrap"
        );
    }
}
