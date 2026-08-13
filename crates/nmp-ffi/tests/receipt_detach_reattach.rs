//! #680 / #46 — receipt facts are durable and reattachable across a detach that
//! happens BEFORE the write reaches its retained terminal/steady state
//! (falsifier item 5). The live async receipt stream is not the source of
//! truth: the persisted publish-queue store is. Detaching a consumer loses no
//! fact — a fresh reattached stream traverses the durable history, including
//! facts that accrued AFTER the original consumer was dropped.
//!
//! Bounded-overflow rule (stated, and structurally enforced): the live receipt
//! FIFO has a fixed capacity of 32. Retry execution is bounded but retry count
//! is not. A slow consumer retains the finite prefix, is disconnected loudly
//! with typed lag, and reattachment traverses deterministic durable pages;
//! correctness does not depend on the in-memory queue. Persisted-history
//! retention/GC remains #46's separate concern.

use std::sync::Arc;
use std::time::Duration;

use nmp_ffi::facade::{NmpEngine, NmpEngineConfig, NmpReceiptStream};
use nmp_ffi::session::{FfiPrivateKey, FfiPublicKey};
use nmp_ffi::types::{
    FfiIdentity, FfiReceiptReattachment, FfiRelayState, FfiSigningState, FfiWriteFact,
    FfiWriteIntent, FfiWriteOutcome, FfiWritePayload, FfiWriteRouting,
};
use nmp_store::{
    AcceptWrite, EventStore, HandoffEvidence, IntentSigState, PublishQueueAttemptHandoff,
    PublishQueueAttemptOutcome, PublishQueueLaneKey, PublishQueuePostHandoffState,
    PublishQueueTransientCause, VerifiedSignature,
};
use nostr::Event;

/// The verified, intent-bound evidence `promote_signed` takes (#768). Every
/// event promoted below is one this fixture just signed itself, so the
/// verification succeeding is part of the setup, not the property under test.
fn evidence(signed: &Event) -> VerifiedSignature {
    VerifiedSignature::verify(signed).expect("fixture events are validly signed")
}

async fn next_status(stream: &Arc<NmpReceiptStream>) -> Option<FfiWriteFact> {
    tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("a receipt status must arrive within 5s")
        .expect("receipt next() is not a misuse")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn receipt_detached_before_terminal_reattaches_full_durable_prefix_from_the_store() {
    let engine = NmpEngine::new(NmpEngineConfig::default(), None).expect("engine builds");
    let keys = nostr::Keys::generate();
    engine
        .add_public_key_account(
            FfiPublicKey::from_bytes(keys.public_key().to_bytes().to_vec()).unwrap(),
            true,
        )
        .expect("activate account");

    // A durable write. With no signing provider configured yet it parks in a
    // retained `Signing { AwaitingSigner }` steady state.
    let receipt = engine
        .publish(FfiWriteIntent {
            payload: FfiWritePayload::Event {
                builder: nmp_ffi::types::FfiEventBuilder {
                    kind: 9999,
                    tags: vec![],
                    content: "detach-before-terminal".to_string(),
                    created_at: Some(nostr::Timestamp::now().as_secs()),
                },
            },
            routing: FfiWriteRouting::Explicit {
                relays: vec!["wss://write.example".to_string()],
            },
            identity: FfiIdentity::Active,
            correlation: None,
        })
        .expect("publish enqueues");
    let receipt_id = receipt.id();
    assert!(receipt_id > 0);

    // Observe only the FIRST fact, then DETACH (drop the live stream) BEFORE the
    // write reaches its terminal.
    //
    // (#1237: acceptance is `publish` returning Ok, not a stream item, so the
    // park itself is now the first fact. The fact that must accrue AFTER the
    // detach is therefore the signature, driven by configuring the provider below.)
    let parked = FfiWriteFact::Signing {
        state: FfiSigningState::AwaitingSigner {
            pubkey: keys.public_key().to_hex(),
        },
    };
    assert_eq!(next_status(&receipt).await, Some(parked.clone()));
    drop(receipt);

    // Advance the write past the detach point: the account's provider becomes
    // available, so the
    // `Signed` fact accrues while NO consumer is attached.
    engine
        .add_private_key_account(
            FfiPrivateKey::from_bytes(keys.secret_key().to_secret_bytes().to_vec()).unwrap(),
            false,
        )
        .expect("configure the provider for the frozen account");
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Reattach through a FRESH stream. It replays the durable prefix from the
    // persisted store — including the fact that accrued after the detach —
    // proving the store, not the dropped live channel, is the source of truth.
    let replay = match engine
        .reattach_receipt(receipt_id)
        .expect("reattach while engine open")
    {
        FfiReceiptReattachment::Attached { stream } => stream,
        FfiReceiptReattachment::NotFound => panic!("expected Attached, got NotFound"),
        FfiReceiptReattachment::RetainedButUnreadable => {
            panic!("expected Attached, got RetainedButUnreadable")
        }
    };

    // The replayed prefix is reconstructed from the durable STATE, so the park
    // — which is no longer true of this write — is not re-emitted: signing is
    // "one signature, one author, one answer", not an append-only log. What
    // matters, and what the dropped in-memory channel could never supply, is
    // that the fact which accrued AFTER the detach is there.
    let replayed = next_status(&replay).await;
    assert!(
        matches!(
            replayed,
            Some(FfiWriteFact::Signing {
                state: FfiSigningState::Signed { .. }
            })
        ),
        "the fact that accrued AFTER detach is recovered from the persisted store, \
         not from any retained in-memory channel; got {replayed:?}"
    );
    assert_ne!(
        replayed,
        Some(parked),
        "the reattached stream must not resurrect a signing state the store has \
         already superseded"
    );

    eprintln!(
        "#680 receipt durability: detached while parked, reattached and replayed the durable \
         prefix (Signed, accrued after the detach) from the persisted store."
    );

    engine.shutdown();
}

/// The FFI stream, not merely `EngineCore`, must cross page boundaries without
/// exposing a second iterator or dropping a fact. Seed a closed durable receipt
/// with 40 real attempt ordinals, then traverse it through the public
/// `NmpReceiptStream.next()` API; each internal page is capped at 32.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ffi_reattachment_transparently_traverses_more_than_one_durable_page() {
    let fixture = tempfile::tempdir().expect("tempdir");
    let path = fixture.path().join("paged-receipt.redb");
    let keys = nostr::Keys::generate();
    let relay = nostr::RelayUrl::parse("wss://paged-receipt.example").unwrap();
    let signed = nostr::EventBuilder::new(nostr::Kind::TextNote, "paged receipt")
        .custom_created_at(nostr::Timestamp::from(1_000u64))
        .sign_with_keys(&keys)
        .expect("sign fixture event");
    let receipt_id = {
        let mut store = nmp_store::RedbStore::open(&path).expect("open store");
        let frozen = nostr::Event::new(
            signed.id,
            signed.pubkey,
            signed.created_at,
            signed.kind,
            signed.tags.clone(),
            signed.content.clone(),
            nmp_store::sentinel_signature(),
        );
        let accepted = store
            .accept_write(AcceptWrite {
                frozen,
                replaceable_base: None,
                monotonic_stamp: false,
                expected_pubkey: keys.public_key(),
                signing_identity_ref: "ffi-paged-replay".into(),
                routing: "auto".into(),
                sig_state: IntentSigState::Pending,
                accepted_at: nostr::Timestamp::from(1_000u64),
                correlation: None,
            })
            .expect("accept durable fixture");
        let intent_id = accepted.journaled_intent_id().expect("intent id");
        let receipt_id = accepted.journaled_receipt_id().expect("receipt id");
        store
            .promote_signed(intent_id, evidence(&signed))
            .expect("promote fixture");
        store
            .record_route_revision(intent_id, [relay.clone()].into_iter().collect())
            .expect("persist route");
        let mut lane = store
            .bootstrap_publish_queue_lanes(intent_id)
            .expect("bootstrap lane")
            .remove(0);
        let key = PublishQueueLaneKey {
            intent_id,
            relay: relay.clone(),
        };
        lane = store
            .set_lane_eligible(&key, lane.revision, nostr::Timestamp::from(1_001u64))
            .expect("arm first attempt");

        for ordinal in 1..=40u64 {
            let base = 1_000 + ordinal * 10;
            let (attempt, started) = store
                .start_lane_attempt(
                    &key,
                    lane.revision,
                    signed.clone(),
                    nostr::Timestamp::from(base),
                )
                .expect("start durable attempt");
            assert_eq!(attempt.ordinal, ordinal);
            if ordinal < 40 {
                lane = store
                    .record_lane_handoff(
                        &key,
                        started.revision,
                        ordinal,
                        PublishQueueAttemptHandoff {
                            at: nostr::Timestamp::from(base + 1),
                            result: HandoffEvidence::Written,
                        },
                        PublishQueuePostHandoffState::Transient {
                            eligible_at: nostr::Timestamp::from(base + 2),
                            cause: PublishQueueTransientCause::RelayRateLimited,
                            raw_reason: Some("ffi bounded-page proof".into()),
                        },
                    )
                    .expect("persist retry");
                lane = store
                    .set_lane_eligible(&key, lane.revision, nostr::Timestamp::from(base + 2))
                    .expect("arm next retry");
            } else {
                let awaiting_ack = store
                    .record_lane_handoff(
                        &key,
                        started.revision,
                        ordinal,
                        PublishQueueAttemptHandoff {
                            at: nostr::Timestamp::from(base + 1),
                            result: HandoffEvidence::Written,
                        },
                        PublishQueuePostHandoffState::AwaitingAck {
                            deadline: nostr::Timestamp::from(base + 2),
                        },
                    )
                    .expect("persist final handoff");
                store
                    .finish_lane_attempt(
                        &key,
                        awaiting_ack.revision,
                        ordinal,
                        PublishQueueAttemptOutcome::GaveUp,
                        nostr::Timestamp::from(base + 2),
                    )
                    .expect("finish final attempt");
            }
        }
        store
            .close_terminal_intent(intent_id)
            .expect("close terminal intent");
        receipt_id
    };

    let engine = NmpEngine::new(
        NmpEngineConfig {
            store_path: Some(path.to_string_lossy().into_owned()),
            ..NmpEngineConfig::default()
        },
        None,
    )
    .expect("open engine over seeded receipt");
    let stream = match engine
        .reattach_receipt(receipt_id)
        .expect("reattach seeded receipt")
    {
        FfiReceiptReattachment::Attached { stream } => stream,
        FfiReceiptReattachment::NotFound => panic!("seeded receipt disappeared"),
        FfiReceiptReattachment::RetainedButUnreadable => {
            panic!("seeded canonical receipt became unreadable")
        }
    };

    let mut replay = Vec::new();
    while let Some(status) = next_status(&stream).await {
        replay.push(status);
    }
    assert!(
        replay.len() > nmp::FACT_CHANNEL_CAPACITY,
        "public FFI traversal must cross at least one internal page"
    );
    assert!(
        replay.iter().any(|status| matches!(
            status,
            FfiWriteFact::Relay {
                state: FfiRelayState::Sent { attempt: 40, .. },
                ..
            }
        )),
        "the final durable ordinal must survive transparent page transitions"
    );
    assert_eq!(
        replay
            .iter()
            .filter(|status| matches!(
                status,
                FfiWriteFact::Relay {
                    state: FfiRelayState::Sent { .. },
                    ..
                }
            ))
            .count(),
        40,
        "every persisted handoff must arrive exactly once"
    );

    let result_stream = match engine
        .reattach_receipt(receipt_id)
        .expect("reattach seeded receipt for terminal reduction")
    {
        FfiReceiptReattachment::Attached { stream } => stream,
        _ => panic!("seeded receipt must remain reattachable"),
    };
    let result = result_stream.result().await.expect("NMP reduces all pages");
    assert_eq!(result.outcome, FfiWriteOutcome::Settled);
    assert_eq!(result.relays.len(), 1);
    assert_eq!(result.relays[0].relay, relay.to_string());
    assert_eq!(result.relays[0].state, FfiRelayState::GaveUp);

    engine.shutdown();
}
