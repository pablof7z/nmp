//! Deterministic falsifiers for identity-receipt observation ordering.
//!
//! The product may return a registered receipt before its first status is
//! dispatched. These tests keep the BDD refusal steps honest about that legal
//! ordering without moving the wait into assertion-message snapshots.

use std::thread;
use std::time::Duration;

use nmp::mechanism::outbox::WriteStatus;
use nmp::mechanism::runtime::fifo_channel;

use super::{NmpWorld, ReceiptState};

#[test]
fn identity_receipt_waits_for_first_status_dispatched_after_publish_returns() {
    let (sender, receiver) = fifo_channel();
    let mut world = NmpWorld::default();
    world.receipts.push(ReceiptState::new(receiver));

    assert!(
        world.identity_receipt_statuses(None).is_empty(),
        "a newly returned receipt may legally be empty before status dispatch"
    );

    let dispatch = thread::spawn(move || {
        thread::sleep(Duration::from_millis(25));
        assert!(sender.send(WriteStatus::Failed("invalid signed event".to_string())));
    });

    assert!(world.identity_receipt_reported_anything(None));
    assert!(matches!(
        world.identity_receipt_statuses(None).as_slice(),
        [WriteStatus::Failed(_)]
    ));
    dispatch
        .join()
        .expect("delayed status dispatch must finish");
}

#[test]
fn immediate_refusal_steps_use_the_bounded_first_status_observer() {
    let sources = [
        include_str!("../steps/then/identity.rs"),
        include_str!("../steps/then/payloads.rs"),
    ];
    let source = sources.join("\n");

    assert_eq!(
        source
            .matches("w.identity_receipt_reported_anything(None)")
            .count(),
        3,
        "all three identity-specific immediate refusals must wait for the first status"
    );
    assert!(
        !source.contains("!w.identity_receipt_statuses(None).is_empty()"),
        "a zero-duration status snapshot must not be restored as a receipt precondition"
    );
}
