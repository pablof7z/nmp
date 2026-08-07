//! Issue #768 falsifiers: promotion requires verified, intent-bound
//! evidence.
//!
//! `EventStore::promote_signed` used to take a bare `Signature` guarded only
//! by a doc sentence. Against that door every case below succeeded: a
//! foreign-but-valid signature promoted, a wholly invalid signature
//! promoted, and — worst — a foreign signature over a pending kind:5 draft
//! promoted, turning provisional suppression claims into PERMANENT
//! tombstones that no compensation can reverse.
//!
//! [`VerifiedSignature`] closes both halves. "Verified" is structural: the
//! value has no public constructor but one that runs `nostr::Event::verify`,
//! so case 2 cannot even be written. "Intent-bound" is a typed refusal at
//! the door: the verified event id must equal the intent's own frozen id, or
//! nothing is mutated at all.

use nmp_store::{
    sentinel_signature, AcceptOutcome, AcceptWrite, EventStore, InsertOutcome, IntentSigState,
    MemoryStore, PersistenceFault, PromoteOutcome, ReceiptState, RedbStore, RefuseReason,
    RelayObserved, SigState, VerifiedSignature,
};
use nostr::{Event, EventBuilder, Filter, Keys, Kind, RelayUrl, Tag, Timestamp};

fn for_each_backend(mut body: impl FnMut(&mut dyn EventStore)) {
    let mut mem = MemoryStore::new();
    body(&mut mem);

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("store.redb");
    let mut redb = RedbStore::open(&path).expect("open redb store");
    body(&mut redb);
}

fn compose(keys: &Keys, kind: Kind, content: &str, created_at: u64) -> (Event, Event) {
    let signed = EventBuilder::new(kind, content)
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(keys)
        .expect("sign event");
    (frozen_from_signed(&signed), signed)
}

fn frozen_from_signed(signed: &Event) -> Event {
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

fn evidence(signed: &Event) -> VerifiedSignature {
    VerifiedSignature::verify(signed).expect("fixture events are validly signed")
}

fn accept(frozen: Event, expected_pubkey: nostr::PublicKey, accepted_at: u64) -> AcceptWrite {
    AcceptWrite {
        frozen,
        replaceable_base: None,
        monotonic_stamp: false,
        expected_pubkey,
        signing_identity_ref: "local".to_string(),
        routing: "auto".to_string(),
        sig_state: IntentSigState::Pending,
        accepted_at: Timestamp::from(accepted_at),
        correlation: None,
    }
}

fn do_accept(store: &mut dyn EventStore, request: AcceptWrite) -> AcceptOutcome {
    store
        .accept_write(request)
        .expect("accept_write persistence")
}

/// Falsifier 1 (intent-bound): verify an event for intent A, then try to
/// promote intent B with that evidence. The signature is perfectly valid —
/// it simply is not this intent's. Typed refusal, and zero mutation of B's
/// row or of its journal.
#[test]
fn a_valid_signature_from_another_intent_is_refused() {
    for_each_backend(|store| {
        let k = Keys::generate();
        let (frozen_a, signed_a) = compose(&k, Kind::TextNote, "alpha", 100);
        let (frozen_b, _signed_b) = compose(&k, Kind::TextNote, "beta", 200);
        let frozen_b_id = frozen_b.id;

        do_accept(store, accept(frozen_a, k.public_key(), 100))
            .journaled_intent_id()
            .expect("A journals an intent");
        let accepted_b = do_accept(store, accept(frozen_b, k.public_key(), 200));
        let intent_b = accepted_b
            .journaled_intent_id()
            .expect("B journals an intent");
        let receipt_b = accepted_b
            .journaled_receipt_id()
            .expect("B journals a receipt");

        let error = store
            .promote_signed(intent_b, evidence(&signed_a))
            .expect_err("a signature over another intent's event must be refused");
        assert_eq!(error.fault(), PersistenceFault::Invariant);

        let rows = store.query(&Filter::new().id(frozen_b_id)).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].event.sig,
            sentinel_signature(),
            "a refused promotion leaves the sentinel in place"
        );
        assert_eq!(
            rows[0].provenance.local.as_ref().unwrap().sig_state,
            SigState::Pending
        );
        assert_eq!(
            store
                .reattach_receipt(receipt_b)
                .expect("receipt readable")
                .expect("receipt retained")
                .state,
            ReceiptState::Accepted,
            "a refused promotion does not advance the receipt"
        );
        // `MemoryStore::recover_publish_queue` is empty by construction
        // (Fable checkpoint Q4), so the durable journal claim is asserted
        // wherever there is a journal to read.
        for record in store.recover_publish_queue().expect("recover") {
            if record.intent_id == intent_b {
                assert_eq!(record.sig_state, IntentSigState::Pending);
                assert!(
                    record.displaced.is_none(),
                    "a refused promotion drops no recovery state"
                );
            }
        }
    });
}

/// Falsifier 2 (verified): a shape-valid but cryptographically invalid
/// signature. There is no `promote_signed` call to make here at all — the
/// evidence the door demands cannot be constructed, which is the whole
/// point of moving the precondition into the type.
#[test]
fn a_cryptographically_invalid_signature_yields_no_evidence() {
    let k = Keys::generate();
    let stranger = Keys::generate();
    let (_frozen, mine) = compose(&k, Kind::TextNote, "mine", 100);
    let (_, theirs) = compose(&stranger, Kind::TextNote, "not mine", 100);

    // My exact bytes and my id, carrying somebody else's signature.
    let forged = Event::new(
        mine.id,
        mine.pubkey,
        mine.created_at,
        mine.kind,
        mine.tags.clone(),
        mine.content.clone(),
        theirs.sig,
    );
    assert!(
        VerifiedSignature::verify(&forged).is_err(),
        "an invalid signature must produce no promotion evidence"
    );
    // A sentinel row's own signature is likewise not evidence of anything.
    assert!(VerifiedSignature::verify(&frozen_from_signed(&mine)).is_err());
    // And the honest article still verifies, so the guard is not vacuous.
    assert_eq!(
        VerifiedSignature::verify(&mine).unwrap().event_id(),
        mine.id
    );
}

/// Falsifier 3: the irreversible one. A pending kind:5 draft holds only a
/// PROVISIONAL suppression claim — a redelivery of the target is still
/// accepted and stored. Promotion is what makes the claim a PERMANENT
/// tombstone, and nothing reverses that afterwards. A promotion refused for
/// mis-bound evidence must not cross that line.
#[test]
fn a_foreign_signature_cannot_commit_a_pending_kind5_to_permanent_tombstones() {
    for_each_backend(|store| {
        let k = Keys::generate();
        let target = EventBuilder::new(Kind::TextNote, "please delete me")
            .custom_created_at(Timestamp::from(50))
            .sign_with_keys(&k)
            .expect("sign target");
        let target_id = target.id;
        let relay = RelayUrl::parse("wss://relay.example").expect("relay url");
        store
            .insert(target, RelayObserved::new(relay, Timestamp::from(50)))
            .unwrap();

        let (unrelated_frozen, unrelated_signed) = compose(&k, Kind::TextNote, "unrelated", 90);
        do_accept(store, accept(unrelated_frozen, k.public_key(), 90))
            .journaled_intent_id()
            .expect("unrelated intent");

        let signed_deletion = EventBuilder::new(Kind::EventDeletion, "")
            .tag(Tag::event(target_id))
            .custom_created_at(Timestamp::from(100))
            .sign_with_keys(&k)
            .expect("sign deletion");
        let deletion_intent = do_accept(
            store,
            accept(frozen_from_signed(&signed_deletion), k.public_key(), 100),
        )
        .journaled_intent_id()
        .expect("deletion journals an intent");

        let error = store
            .promote_signed(deletion_intent, evidence(&unrelated_signed))
            .expect_err("a foreign signature must not commit the deletion");
        assert_eq!(error.fault(), PersistenceFault::Invariant);

        // Still provisional: a redelivered target is accepted, not refused.
        let redelivered = EventBuilder::new(Kind::TextNote, "please delete me")
            .custom_created_at(Timestamp::from(50))
            .sign_with_keys(&k)
            .expect("sign redelivered target");
        assert_eq!(redelivered.id, target_id);
        let relay2 = RelayUrl::parse("wss://relay2.example").expect("relay url");
        let reinsert = store
            .insert(
                redelivered,
                RelayObserved::new(relay2, Timestamp::from(300)),
            )
            .unwrap();
        assert!(
            !matches!(reinsert, InsertOutcome::Refused(RefuseReason::Tombstoned)),
            "a refused promotion must not have written permanent tombstones, got {reinsert:?}"
        );

        // And the intent's own promotion is still available to its real
        // signature — the refusal cost it nothing.
        assert!(
            matches!(
                store
                    .promote_signed(deletion_intent, evidence(&signed_deletion))
                    .expect("promote persistence"),
                PromoteOutcome::Promoted { .. }
            ),
            "the real signature still promotes after a refusal"
        );
    });
}
