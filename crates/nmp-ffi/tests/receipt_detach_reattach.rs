//! #680 / #46 — receipt facts are durable and reattachable across a detach that
//! happens BEFORE the write reaches its retained terminal/steady state
//! (falsifier item 5). The live async receipt stream is not the source of
//! truth: the persisted outbox/redb store is. Detaching a consumer loses no
//! fact — a fresh reattached stream replays the full durable prefix, including
//! facts that accrued AFTER the original consumer was dropped.
//!
//! Bounded-overflow rule (stated, and structurally enforced): the live receipt
//! FIFO is bounded by the write's finite `(relay, attempt, state)` lifecycle —
//! `max_relays` live lanes, the 32-global/1-per-relay retry caps, and a fixed
//! set of terminal states — never an unbounded external producer. If a slow or
//! detached consumer falls behind, correctness does not depend on that in-memory
//! queue: every terminal/durable fact is persisted and replayed on reattach
//! (see `Handle::reattach_receipt` -> core `reattach_receipt` -> the store's
//! `reattach_receipt` durable-prefix reconstruction).

use std::sync::Arc;
use std::time::Duration;

use nmp_ffi::facade::{NmpEngine, NmpEngineConfig, NmpReceiptStream};
use nmp_ffi::types::{
    FfiDurability, FfiReceiptReattachment, FfiWriteIntent, FfiWritePayload, FfiWriteRouting,
    FfiWriteStatus,
};

async fn next_status(stream: &Arc<NmpReceiptStream>) -> Option<FfiWriteStatus> {
    tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("a receipt status must arrive within 5s")
        .expect("receipt next() is not a misuse")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn receipt_detached_before_terminal_reattaches_full_durable_prefix_from_the_store() {
    let engine = NmpEngine::new(NmpEngineConfig::default()).expect("engine builds");
    let keys = nostr::Keys::generate();
    engine
        .set_active_account(Some(keys.public_key().to_hex()))
        .expect("activate account");

    // A durable write. With no signer capability ever attaching, it settles into
    // a retained Accepted + AwaitingCapability durable steady state.
    let receipt = engine
        .publish(FfiWriteIntent {
            payload: FfiWritePayload::Unsigned {
                pubkey: keys.public_key().to_hex(),
                created_at: nostr::Timestamp::now().as_secs(),
                kind: 9999,
                tags: vec![],
                content: "detach-before-terminal".to_string(),
            },
            durability: FfiDurability::Durable,
            routing: FfiWriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: None,
        })
        .expect("publish enqueues");
    let receipt_id = receipt.id();
    assert!(receipt_id > 0);

    // Observe only the FIRST fact, then DETACH (drop the live stream) BEFORE the
    // write reaches its retained steady state.
    assert_eq!(next_status(&receipt).await, Some(FfiWriteStatus::Accepted));
    drop(receipt);

    // Let the engine advance the write past the detach point (the
    // AwaitingCapability fact accrues while NO consumer is attached).
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Reattach through a FRESH stream. It replays the FULL durable prefix from
    // the persisted store — including the fact that accrued after the detach —
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

    assert_eq!(
        next_status(&replay).await,
        Some(FfiWriteStatus::Accepted),
        "replay reconstructs the Accepted fact from the store"
    );
    assert_eq!(
        next_status(&replay).await,
        Some(FfiWriteStatus::AwaitingCapability {
            pubkey: keys.public_key().to_hex()
        }),
        "the fact that accrued AFTER detach is recovered from the persisted store, \
         not from any retained in-memory channel"
    );

    eprintln!(
        "#680 receipt durability: detached before terminal, reattached and replayed the full \
         durable prefix (Accepted, AwaitingCapability) from the persisted store."
    );

    engine.shutdown();
}
