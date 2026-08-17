//! #1782 falsifiers for the owner ruling: signature verification happens
//! ONCE, at ingest, and no stored event is ever re-verified.
//!
//! The evidence is a COUNT, not a duration. [`crate::schnorr_verifications`]
//! reports every schnorr check [`crate::VerifiedSignature::verify`] — the
//! crate's only schnorr entry point — has run on this thread. Population
//! moves it once per promoted event; a boot over that same store must not
//! move it at all.
//!
//! Before this change the same boot ran one schnorr check per attempt row
//! (`decode_attempt`), so the zero-assertion below is a real falsifier: put
//! the check back and it goes red with the exact attempt-row count.
//!
//! The counter's honest limit: it observes verification spelled through
//! `VerifiedSignature::verify`. A raw `nostr::Event::verify` reintroduced
//! somewhere in this crate would evade it. What makes that limit tolerable
//! is that after #1782 there is no such call left in `nmp-store` at all —
//! `VerifiedSignature::verify` is the single spelling, and it is only ever
//! reached from `nmp-engine`.

use nostr::{Event, EventBuilder, Keys, Kind, RelayUrl, Timestamp};
use tempfile::TempDir;

use super::store::RedbStore;
use crate::{
    sentinel_signature, AcceptWrite, AcceptWritePayload, HandoffEvidence, IntentSigState,
    PromotionTarget, PublishQueueAttemptHandoff, PublishQueuePostHandoffState,
    PublishQueueTransientCause, PublishQueueWork, RelayObserved, VerifiedSignature,
};

/// Open intents populated by [`populate`]. Each contributes `RELAYS` lanes,
/// and each lane one attempt row.
const INTENTS: usize = 50;
/// Delivery lanes per intent — so `INTENTS * RELAYS` attempt rows, plus
/// `INTENTS` relay-observed rows on top, exercise the boot path.
const RELAYS: usize = 4;
/// Relay-observed events inserted alongside, so the boot walks a store with
/// a realistic event population rather than only publish-queue rows.
const OBSERVED: usize = 10_000;

fn keys() -> Keys {
    Keys::parse("000000000000000000000000000000000000000000000000000000000000002a")
        .expect("fixed fixture key")
}

fn signed_event(keys: &Keys, index: usize) -> Event {
    EventBuilder::new(Kind::TextNote, format!("boot-verification-{index:08}"))
        .custom_created_at(Timestamp::from(1_000_000 + index as u64))
        .sign_with_keys(keys)
        .expect("sign fixture event")
}

fn frozen_event(signed: &Event) -> Event {
    Event::new(
        signed.id,
        signed.pubkey,
        signed.created_at,
        signed.kind,
        signed.tags.clone(),
        signed.content.clone(),
        sentinel_signature(),
    )
}

fn relay(index: usize) -> RelayUrl {
    RelayUrl::parse(&format!("wss://boot-{index:04}.invalid")).expect("fixture relay")
}

/// Build a store that a boot has real work to walk: `INTENTS` open intents,
/// each promoted, routed to `RELAYS` relays, with one started attempt per
/// lane, plus `OBSERVED` relay-delivered rows.
fn populate(path: &std::path::Path) {
    let keys = keys();
    let mut store = RedbStore::open(path).expect("open fixture store");

    for chunk in 0..OBSERVED / 500 {
        let batch = (0..500)
            .map(|offset| {
                let index = chunk * 500 + offset;
                (
                    signed_event(&keys, 1_000_000 + index),
                    RelayObserved::new(relay(0), Timestamp::from(2_000_000 + index as u64)),
                )
            })
            .collect();
        store.insert_batch(batch).expect("insert observed batch");
    }

    for index in 0..INTENTS {
        let signed = signed_event(&keys, index);
        let accepted = store
            .accept_write(AcceptWrite {
                payload: AcceptWritePayload::Event {
                    frozen: Box::new(frozen_event(&signed)),
                    routing: "boot-verification-fixture".into(),
                    sig_state: IntentSigState::Pending,
                },
                expected_pubkey: keys.public_key(),
                signing_identity_ref: "boot-verification".into(),
                accepted_at: Timestamp::from(3_000_000 + index as u64),
            })
            .expect("accept fixture write");
        let intent_id = accepted.journaled_intent_id().expect("accepted intent");
        store
            .promote_signed(
                PromotionTarget::Event(intent_id),
                VerifiedSignature::verify(&signed).expect("fixture events are validly signed"),
            )
            .expect("promote fixture write");
        store
            .record_route_revision(intent_id, (0..RELAYS).map(relay).collect())
            .expect("record fixture route");
        for lane in store
            .bootstrap_publish_queue_lanes(intent_id)
            .expect("bootstrap fixture lanes")
        {
            let eligible = store
                .set_lane_eligible(
                    &lane.key,
                    lane.revision,
                    Timestamp::from(4_000_000 + index as u64),
                )
                .expect("fixture lane eligible");
            let (attempt, in_flight) = store
                .start_lane_attempt(
                    &lane.key,
                    eligible.revision,
                    signed.clone(),
                    Timestamp::from(5_000_000 + index as u64),
                )
                .expect("start fixture attempt");
            store
                .record_lane_handoff(
                    &lane.key,
                    in_flight.revision,
                    attempt.ordinal,
                    PublishQueueAttemptHandoff {
                        at: Timestamp::from(6_000_000 + index as u64),
                        result: HandoffEvidence::Ambiguous,
                    },
                    PublishQueuePostHandoffState::Transient {
                        eligible_at: Timestamp::from(7_000_000 + index as u64),
                        cause: PublishQueueTransientCause::ConnectionLost,
                        raw_reason: Some("fixture transient".into()),
                    },
                )
                .expect("record fixture handoff");
        }
    }
}

/// Everything an engine thread does to a freshly reopened store before it can
/// schedule: recover the open intents, their lanes, their route revisions,
/// their attempt rows and details. Returns the attempt rows walked, so the
/// zero-assertion cannot pass vacuously over an empty store.
fn boot(path: &std::path::Path) -> usize {
    let store = RedbStore::open(path).expect("reopen fixture store");
    let mut attempt_rows = 0usize;
    for intent in store
        .recover_publish_queue()
        .expect("recover publish queue")
    {
        assert!(matches!(intent.work, PublishQueueWork::Event { .. }));
        store
            .recover_publish_queue_lanes(intent.intent_id)
            .expect("recover lanes");
        store
            .recover_route_revisions(intent.intent_id)
            .expect("recover route revisions");
        attempt_rows += store
            .recover_attempts(intent.intent_id)
            .expect("recover attempts")
            .len();
        store
            .recover_attempt_details(intent.intent_id)
            .expect("recover attempt details");
    }
    attempt_rows
}

/// The ruling, as a number: a boot over a populated store performs ZERO
/// schnorr checks. Before #1782 this boot performed exactly one per attempt
/// row.
#[test]
fn boot_over_a_populated_store_performs_zero_signature_verifications() {
    let dir = TempDir::new().expect("fixture directory");
    let path = dir.path().join("boot-verification.redb");
    populate(&path);

    let before = crate::schnorr_verifications();
    let attempt_rows = boot(&path);
    let after = crate::schnorr_verifications();

    assert_eq!(
        attempt_rows,
        INTENTS * RELAYS,
        "NOTHING TO OBSERVE -- the boot walked no attempt rows, so a zero \
         verification count would be vacuous"
    );
    assert_eq!(
        after - before,
        0,
        "a boot over {INTENTS} open intents ({attempt_rows} attempt rows) and \
         {OBSERVED} stored events must perform ZERO signature verifications; it \
         performed {}",
        after - before
    );
}

/// Population DOES verify — once per promoted event, at the boundary. Without
/// this, the zero above could be satisfied by instrumentation that never
/// counts anything.
#[test]
fn promotion_verifies_exactly_once_per_event() {
    let dir = TempDir::new().expect("fixture directory");
    let path = dir.path().join("promotion-count.redb");
    let keys = keys();
    let mut store = RedbStore::open(&path).expect("open fixture store");

    let before = crate::schnorr_verifications();
    for index in 0..8usize {
        let signed = signed_event(&keys, 500_000 + index);
        let accepted = store
            .accept_write(AcceptWrite {
                payload: AcceptWritePayload::Event {
                    frozen: Box::new(frozen_event(&signed)),
                    routing: "boot-verification-fixture".into(),
                    sig_state: IntentSigState::Pending,
                },
                expected_pubkey: keys.public_key(),
                signing_identity_ref: "boot-verification".into(),
                accepted_at: Timestamp::from(3_000_000 + index as u64),
            })
            .expect("accept fixture write");
        store
            .promote_signed(
                PromotionTarget::Event(accepted.journaled_intent_id().expect("accepted intent")),
                VerifiedSignature::verify(&signed).expect("fixture events are validly signed"),
            )
            .expect("promote fixture write");
    }
    let after = crate::schnorr_verifications();

    assert_eq!(
        after - before,
        8,
        "eight promoted events must mint eight verifications -- one each, at \
         the boundary"
    );
}

/// The ruling removes RE-verification, not verification. A forged signature
/// still cannot mint the evidence `promote_signed` demands, so it can never
/// reach a durable row in the first place.
#[test]
fn a_forged_signature_is_still_refused_at_the_boundary() {
    let keys = keys();
    let honest = signed_event(&keys, 42);

    // Same body, one content byte changed: the id no longer matches, and the
    // signature covers neither.
    let forged = Event::new(
        honest.id,
        honest.pubkey,
        honest.created_at,
        honest.kind,
        honest.tags.clone(),
        "boot-verification-forged",
        honest.sig,
    );
    assert!(
        VerifiedSignature::verify(&forged).is_err(),
        "a body that the signature does not cover must not mint promotion evidence"
    );

    // A real signature for a DIFFERENT event, transplanted onto this one.
    let other = signed_event(&keys, 43);
    let transplanted = Event::new(
        honest.id,
        honest.pubkey,
        honest.created_at,
        honest.kind,
        honest.tags.clone(),
        honest.content.clone(),
        other.sig,
    );
    assert!(
        VerifiedSignature::verify(&transplanted).is_err(),
        "a valid signature belonging to another event must not mint promotion evidence"
    );

    // And the sentinel a pending row carries is not a signature at all.
    let pending = frozen_event(&honest);
    assert!(
        VerifiedSignature::verify(&pending).is_err(),
        "the sentinel signature must not mint promotion evidence"
    );

    assert!(
        VerifiedSignature::verify(&honest).is_ok(),
        "NOTHING TO OBSERVE -- the honest fixture must verify, or the three \
         refusals above prove nothing about forgery"
    );
}
