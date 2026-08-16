//! Deterministic falsifiers for identity-receipt observation ordering.
//!
//! The product may return a registered receipt before its first status is
//! dispatched. These tests keep the BDD refusal steps honest about that legal
//! ordering without moving the wait into assertion-message snapshots.

use std::thread;
use std::time::Duration;

use nmp_engine::publish_queue::{SigningState, WriteFact};
use nmp_runtime::fifo_channel;

use super::{NmpWorld, ReceiptState};

#[test]
fn identity_receipt_waits_for_first_fact_dispatched_after_publish_returns() {
    let (sender, receiver) = fifo_channel();
    let mut world = NmpWorld::default();
    world.receipts.push(ReceiptState::new(receiver));

    assert!(
        world.identity_receipt_statuses(None).is_empty(),
        "a newly returned receipt may legally be empty before fact dispatch"
    );

    let dispatch = thread::spawn(move || {
        thread::sleep(Duration::from_millis(25));
        assert!(sender.send(WriteFact::Signing(SigningState::Refused {
            reason: "the signer said no".to_string(),
        })));
    });

    assert!(world.identity_receipt_reported_anything(None));
    assert!(matches!(
        world.identity_receipt_statuses(None).as_slice(),
        [WriteFact::Signing(SigningState::Refused { .. })]
    ));
    dispatch.join().expect("delayed fact dispatch must finish");
}

/// A refusal is `publish()` answering `Err`, and it is known the instant the
/// call returns: there is no stream, no first fact and nothing to wait for.
/// So a refusal step reads the recorded error rather than the receipt.
#[test]
fn immediate_refusal_steps_read_the_doors_answer_not_the_stream() {
    let sources = [
        include_str!("../steps/then/identity.rs"),
        include_str!("../steps/then/payloads.rs"),
    ];
    let source = sources.join("\n");

    assert_eq!(
        source
            .matches("w.identity_receipt_reported_anything(None)")
            .count(),
        0,
        "a door refusal produces no fact, so waiting for one would wait for ever"
    );
    assert_eq!(
        source
            .matches("w.write_refused_before_acceptance(None)")
            .count(),
        3,
        "all three identity-specific immediate refusals must read the publish door's own \
         answer"
    );
}
