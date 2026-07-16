//! Genuine redb close/reopen falsifiers for issues #2/#3 U4. No sleeps,
//! retry timers, or polling: restart is represented by dropping the whole
//! reducer/store and opening the database again.

use std::borrow::Cow;
use std::sync::{Arc, Mutex};

use nmp_engine::core::{
    AuthCapability, AuthCapabilityInstance, AuthEffect, AuthPolicyOutcome, AuthSendOutcome,
    AuthSignerOutcome, Effect, EngineCore, EngineMsg, ReattachOutcome, ReceiptId,
};
use nmp_engine::outbox::{ReceiptSink, WriteStatus};
use nmp_grammar::{
    AccessContext, Durability, HostAuthority, RelaySessionKey, WriteIntent, WritePayload,
    WriteRouting,
};
use nmp_router::FixtureDirectory;
use nmp_store::{
    sentinel_signature, AcceptWrite, AttemptOutcome, EventStore, IntentSigState, RedbStore,
    SigState, WriteDurability,
};
use nmp_transport::{HandoffResult, RelayFrame, RelayHandle};
use nostr::{
    EventBuilder, Keys, Kind, PublicKey, RelayMessage, RelayUrl, Timestamp, UnsignedEvent,
};
use redb::{Database, ReadableTable, TableDefinition};

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<WriteStatus>>>);

impl ReceiptSink for Sink {
    fn on_status(&self, status: WriteStatus) {
        self.0.lock().unwrap().push(status);
    }
}

fn receipt_id(effects: &[Effect]) -> ReceiptId {
    effects
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitReceipt(id, WriteStatus::Accepted) => Some(*id),
            _ => None,
        })
        .expect("accepted receipt")
}

fn signed(keys: &Keys, content: &str, created_at: u64) -> nostr::Event {
    EventBuilder::new(Kind::TextNote, content)
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(keys)
        .unwrap()
}

fn directory(pk: PublicKey, relay: RelayUrl) -> FixtureDirectory {
    FixtureDirectory::new().with_write(pk.to_hex(), [relay])
}

// With the #8 AUTH reducer landed, the write plane rides the signing
// identity's authenticated session again: every durable write demands
// `AccessContext::Nip42(signing pubkey)`, so restart falsifiers that expect
// attempts must connect exactly this session.
fn signer_session(relay: &RelayUrl, signer: PublicKey) -> RelaySessionKey {
    RelaySessionKey::new(relay.clone(), AccessContext::Nip42(signer))
}

/// Complete the canonical NIP-42 handshake for one exact connected session.
/// The returned effects are the matching AUTH `OK` wake.
fn authenticate(
    core: &mut EngineCore<RedbStore>,
    handle: RelayHandle,
    session: &RelaySessionKey,
    signer: &Keys,
) -> Vec<Effect> {
    let challenge = core.handle(EngineMsg::RelayFrame(
        handle,
        session.clone(),
        RelayFrame::from(RelayMessage::Auth {
            challenge: Cow::Owned(format!("durable-restart-{}", handle.slot)),
        }),
    ));
    let policy_token = challenge
        .into_iter()
        .find_map(|effect| match effect {
            Effect::RelayAuth(AuthEffect::RequestPolicy { token, .. }) => Some(token),
            _ => None,
        })
        .expect("AUTH challenge requests exact-session policy");
    assert_eq!(policy_token.epoch.session, *session);
    assert_eq!(policy_token.epoch.handle, handle);

    let policy_instance = AuthCapabilityInstance(1);
    core.handle(EngineMsg::AuthCapabilityBound {
        token: policy_token.clone(),
        capability: AuthCapability::Policy,
        instance: policy_instance,
    });
    let signature = core.handle(EngineMsg::AuthPolicyCompleted(
        policy_token,
        Some(policy_instance),
        AuthPolicyOutcome::Allow,
    ));
    let (sign_token, unsigned) = signature
        .into_iter()
        .find_map(|effect| match effect {
            Effect::RelayAuth(AuthEffect::RequestSignature { token, unsigned }) => {
                Some((token, unsigned))
            }
            _ => None,
        })
        .expect("allowed AUTH policy requests signature");
    assert_eq!(sign_token.epoch.session, *session);
    assert_eq!(sign_token.epoch.handle, handle);
    assert_eq!(unsigned.kind, Kind::Authentication);
    assert_eq!(unsigned.pubkey, signer.public_key());

    let signed = unsigned.sign_with_keys(signer).unwrap();
    let signer_instance = AuthCapabilityInstance(2);
    core.handle(EngineMsg::AuthCapabilityBound {
        token: sign_token.clone(),
        capability: AuthCapability::Signer,
        instance: signer_instance,
    });
    let send = core.handle(EngineMsg::AuthSignerCompleted(
        sign_token,
        Some(signer_instance),
        AuthSignerOutcome::Signed(signed),
    ));
    let (send_token, auth_event) = send
        .into_iter()
        .find_map(|effect| match effect {
            Effect::RelayAuth(AuthEffect::Send {
                token,
                epoch,
                event,
            }) => {
                assert_eq!(epoch.session, *session);
                assert_eq!(epoch.handle, handle);
                Some((token, event))
            }
            _ => None,
        })
        .expect("signed AUTH requests exact-generation send");
    core.handle(EngineMsg::AuthSendCompleted(
        send_token,
        AuthSendOutcome::Accepted,
    ));
    core.handle(EngineMsg::RelayFrame(
        handle,
        session.clone(),
        RelayFrame::from(RelayMessage::ok(auth_event.id, true, "authenticated")),
    ))
}

fn strip_additive_lane_rows(path: &std::path::Path, intent: nmp_store::IntentId, relay: &RelayUrl) {
    let db = Database::open(path).unwrap();
    let write = db.begin_write().unwrap();
    {
        let details: TableDefinition<&str, &str> = TableDefinition::new("outbox_attempt_details");
        let lanes: TableDefinition<&str, &str> = TableDefinition::new("outbox_lanes");
        let mut details = write.open_table(details).unwrap();
        let mut lanes = write.open_table(lanes).unwrap();
        let attempt_prefix = format!(
            "{:020}:{:020}:{}",
            intent.0,
            relay.as_str().len(),
            relay.as_str()
        );
        let canonical: &nostr::Url = relay.into();
        let canonical = canonical.as_str();
        let lane_key = format!("{:020}:{:020}:{canonical}", intent.0, canonical.len());
        assert!(details
            .remove(format!("{attempt_prefix}:{:020}", 1).as_str())
            .unwrap()
            .is_some());
        assert!(lanes.remove(lane_key.as_str()).unwrap().is_some());
    }
    write.commit().unwrap();
}

#[test]
fn durable_started_attempt_replays_exact_bytes_and_same_receipt_without_accepting_again() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("durable.redb");
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://durable.example").unwrap();
    let appended = RelayUrl::parse("wss://appended-after-restart.example").unwrap();
    let event = signed(&keys, "exact", 100);
    let relay_session = signer_session(&relay, event.pubkey);
    let appended_session = signer_session(&appended, event.pubkey);

    let id = {
        let store = RedbStore::open(&path).unwrap();
        let mut core = EngineCore::new(
            store,
            Box::new(directory(keys.public_key(), relay.clone())),
            10,
        );
        let handle = RelayHandle {
            slot: 0,
            generation: 1,
        };
        core.handle(EngineMsg::RelayConnected(handle, relay_session.clone()));
        authenticate(&mut core, handle, &relay_session, &keys);
        let effects = core.handle(EngineMsg::Publish(
            WriteIntent {
                payload: WritePayload::Signed(event.clone()),
                durability: Durability::Durable,
                routing: WriteRouting::AuthorOutbox,
                identity_override: None,
            },
            Box::new(Sink::default()),
        ));
        assert!(effects.iter().any(|effect| matches!(effect,
            Effect::PublishEvent(r, e, _) if r == &relay_session && e == &event
        )));
        receipt_id(&effects)
    };
    let intent = RedbStore::open(&path)
        .unwrap()
        .reattach_receipt(id.0)
        .unwrap()
        .unwrap()
        .intent_id
        .unwrap();
    strip_additive_lane_rows(&path, intent, &relay);

    let store = RedbStore::open(&path).unwrap();
    let mut core = EngineCore::new(
        store,
        Box::new(FixtureDirectory::new().with_write(
            keys.public_key().to_hex(),
            [relay.clone(), appended.clone()],
        )),
        10,
    );
    let recovery = core.recover_on_boot();
    assert!(
        recovery
            .iter()
            .any(|effect| matches!(effect, Effect::EnsureRelay(r) if r == &relay_session))
            && recovery
                .iter()
                .any(|effect| matches!(effect, Effect::EnsureRelay(r) if r == &appended_session)),
        "recovery preserves both lanes but allocates no attempt while offline"
    );
    assert!(
        !recovery
            .iter()
            .any(|effect| matches!(effect, Effect::EmitReceipt(_, WriteStatus::Accepted))),
        "boot recovery must not accept the write a second time"
    );

    let first = Sink::default();
    let second = Sink::default();
    assert!(core
        .reattach_receipt(id, Box::new(first.clone()))
        .is_attached());
    assert!(core
        .reattach_receipt(id, Box::new(second.clone()))
        .is_attached());
    assert_eq!(
        first.0.lock().unwrap().len(),
        second.0.lock().unwrap().len()
    );
    assert!(
        !first
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|s| matches!(s, WriteStatus::Sent { relay: r, .. } if r == &relay)),
        "a recovered Started attempt predates transport Written and cannot replay as Sent"
    );
    let relay_handle = RelayHandle {
        slot: 0,
        generation: 1,
    };
    core.handle(EngineMsg::RelayConnected(
        relay_handle,
        relay_session.clone(),
    ));
    let relay_retry = authenticate(&mut core, relay_handle, &relay_session, &keys);
    assert!(relay_retry.iter().any(|effect| matches!(effect,
        Effect::PublishEvent(r, e, _) if r == &relay_session && e == &event
    )));
    let appended_handle = RelayHandle {
        slot: 1,
        generation: 1,
    };
    core.handle(EngineMsg::RelayConnected(
        appended_handle,
        appended_session.clone(),
    ));
    let appended_first = authenticate(&mut core, appended_handle, &appended_session, &keys);
    assert!(appended_first.iter().any(|effect| matches!(effect,
        Effect::PublishEvent(r, e, _) if r == &appended_session && e == &event
    )));
    let correlation = relay_retry
        .iter()
        .find_map(|effect| match effect {
            Effect::PublishEvent(r, _, correlation) if r == &relay_session => Some(*correlation),
            _ => None,
        })
        .unwrap();
    core.handle(EngineMsg::EventHandoff(correlation, HandoffResult::Written));
    let acked = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        relay_session.clone(),
        RelayFrame::from(RelayMessage::ok(event.id, true, "")),
    ));
    assert!(acked.iter().any(|effect| matches!(
        effect,
        Effect::EmitReceipt(receipt, WriteStatus::Acked(acked_relay))
            if *receipt == id && acked_relay == &relay
    )));
    drop(core);
    let store = RedbStore::open(&path).unwrap();
    let original_attempts = store
        .recover_attempts(intent)
        .unwrap()
        .into_iter()
        .filter(|attempt| attempt.relay == relay)
        .collect::<Vec<_>>();
    assert_eq!(
        original_attempts
            .iter()
            .map(|attempt| (attempt.ordinal, &attempt.outcome))
            .collect::<Vec<_>>(),
        vec![(1, &AttemptOutcome::Started), (2, &AttemptOutcome::Acked)],
        "restart preserves the interrupted ordinal and ACKs a new retry ordinal"
    );
    let original_lane = store
        .recover_outbox_lanes(intent)
        .unwrap()
        .into_iter()
        .find(|lane| lane.key.relay == relay)
        .unwrap();
    assert_eq!(
        original_lane.state,
        nmp_store::LaneState::Terminal {
            ordinal: 2,
            outcome: AttemptOutcome::Acked,
        }
    );
}

#[test]
fn at_most_once_started_attempt_becomes_outcome_unknown_and_is_never_resent() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("amo.redb");
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://amo.example").unwrap();
    let event = signed(&keys, "once", 101);
    let session = signer_session(&relay, event.pubkey);
    let intent_id = {
        let store = RedbStore::open(&path).unwrap();
        let mut core = EngineCore::new(
            store,
            Box::new(directory(keys.public_key(), relay.clone())),
            10,
        );
        let handle = RelayHandle {
            slot: 0,
            generation: 1,
        };
        core.handle(EngineMsg::RelayConnected(handle, session.clone()));
        authenticate(&mut core, handle, &session, &keys);
        let effects = core.handle(EngineMsg::Publish(
            WriteIntent {
                payload: WritePayload::Signed(event),
                durability: Durability::AtMostOnce,
                routing: WriteRouting::AuthorOutbox,
                identity_override: None,
            },
            Box::new(Sink::default()),
        ));
        let id = receipt_id(&effects);
        // Resolve the stable receipt to its intent after the reducer drops.
        drop(core);
        RedbStore::open(&path)
            .unwrap()
            .reattach_receipt(id.0)
            .unwrap()
            .unwrap()
            .intent_id
            .unwrap()
    };
    strip_additive_lane_rows(&path, intent_id, &relay);

    let store = RedbStore::open(&path).unwrap();
    let mut core = EngineCore::new(
        store,
        Box::new(directory(keys.public_key(), relay.clone())),
        10,
    );
    let recovery = core.recover_on_boot();
    assert!(!recovery
        .iter()
        .any(|effect| matches!(effect, Effect::PublishEvent(..))));
    drop(core);

    let store = RedbStore::open(&path).unwrap();
    let attempts = store.recover_attempts(intent_id).unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].outcome, AttemptOutcome::OutcomeUnknown);
}

#[test]
fn pending_row_and_frozen_signer_resume_after_reopen_then_cancel_compensates() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("signer.redb");
    let keys = Keys::generate();
    let wrong = Keys::generate();
    let relay = RelayUrl::parse("wss://signer.example").unwrap();
    let unsigned = UnsignedEvent::new(
        keys.public_key(),
        Timestamp::from(102u64),
        Kind::TextNote,
        vec![],
        "resume",
    );
    let frozen_id = nostr::EventId::new(
        &unsigned.pubkey,
        &unsigned.created_at,
        &unsigned.kind,
        &unsigned.tags,
        &unsigned.content,
    );
    let id = {
        let store = RedbStore::open(&path).unwrap();
        let mut core = EngineCore::new(
            store,
            Box::new(directory(keys.public_key(), relay.clone())),
            10,
        );
        core.handle(EngineMsg::SetActivePubkey(Some(keys.public_key())));
        let effects = core.handle(EngineMsg::Publish(
            WriteIntent {
                payload: WritePayload::Unsigned(unsigned),
                durability: Durability::Durable,
                routing: WriteRouting::AuthorOutbox,
                identity_override: None,
            },
            Box::new(Sink::default()),
        ));
        receipt_id(&effects)
    };

    let store = RedbStore::open(&path).unwrap();
    let rows = store.query(&nostr::Filter::new().id(frozen_id)).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].provenance.local.as_ref().unwrap().sig_state,
        SigState::Pending
    );
    let mut core = EngineCore::new(store, Box::new(directory(keys.public_key(), relay)), 10);
    assert!(core.recover_on_boot().is_empty());
    let reattached = Sink::default();
    assert!(core
        .reattach_receipt(id, Box::new(reattached.clone()))
        .is_attached());
    assert_eq!(
        *reattached.0.lock().unwrap(),
        vec![WriteStatus::Accepted, WriteStatus::AwaitingCapability]
    );
    assert!(!core
        .handle(EngineMsg::SignerAttached(wrong.public_key()))
        .iter()
        .any(|e| matches!(e, Effect::RequestSign(..))));
    assert!(core
        .handle(EngineMsg::SignerAttached(keys.public_key()))
        .iter()
        .any(|e| matches!(e, Effect::RequestSign(request_id, _, u)
            if *request_id == id && u.pubkey == keys.public_key())));
    core.handle(EngineMsg::CancelWrite(id));
    drop(core);
    let store = RedbStore::open(&path).unwrap();
    assert!(store
        .query(&nostr::Filter::new().id(frozen_id))
        .unwrap()
        .is_empty());
}

/// #47 falsifier (f), modeled on
/// [`pending_row_and_frozen_signer_resume_after_reopen_then_cancel_compensates`]:
/// an unsigned intent accepted under an explicit `identity_override`
/// (authored by B while A was the active account, B's signer absent)
/// survives a genuine close/reopen still pinned to B. Replay shows
/// `Accepted` + `AwaitingCapability`; re-rooting the reopened core onto A
/// and attaching A's (wrong) signer produce no sign request; attaching the
/// EXACT override key resumes the SAME receipt, and B's completion promotes
/// the frozen body/id/pubkey to `Signed`.
#[test]
fn overridden_unsigned_intent_replays_and_resumes_pinned_to_override_after_reopen() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("override-signer.redb");
    let active = Keys::generate();
    let override_keys = Keys::generate();
    let relay = RelayUrl::parse("wss://override-signer.example").unwrap();
    let unsigned = UnsignedEvent::new(
        override_keys.public_key(),
        Timestamp::from(147u64),
        Kind::TextNote,
        vec![],
        "resume as the override identity",
    );
    let frozen_id = nostr::EventId::new(
        &unsigned.pubkey,
        &unsigned.created_at,
        &unsigned.kind,
        &unsigned.tags,
        &unsigned.content,
    );
    let id = {
        let store = RedbStore::open(&path).unwrap();
        let mut core = EngineCore::new(
            store,
            Box::new(directory(override_keys.public_key(), relay.clone())),
            10,
        );
        // A is the active account; the override alone authorizes B's draft.
        core.handle(EngineMsg::SetActivePubkey(Some(active.public_key())));
        let effects = core.handle(EngineMsg::Publish(
            WriteIntent {
                payload: WritePayload::Unsigned(unsigned),
                durability: Durability::Durable,
                routing: WriteRouting::AuthorOutbox,
                identity_override: Some(override_keys.public_key()),
            },
            Box::new(Sink::default()),
        ));
        receipt_id(&effects)
    };

    // Restart: the frozen pending row is B's body with a pending signature.
    let store = RedbStore::open(&path).unwrap();
    let rows = store.query(&nostr::Filter::new().id(frozen_id)).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].event.pubkey, override_keys.public_key());
    assert_eq!(
        rows[0].provenance.local.as_ref().unwrap().sig_state,
        SigState::Pending
    );
    let mut core = EngineCore::new(
        store,
        Box::new(directory(override_keys.public_key(), relay)),
        10,
    );
    assert!(core.recover_on_boot().is_empty());
    let reattached = Sink::default();
    assert!(core
        .reattach_receipt(id, Box::new(reattached.clone()))
        .is_attached());
    assert_eq!(
        *reattached.0.lock().unwrap(),
        vec![WriteStatus::Accepted, WriteStatus::AwaitingCapability]
    );

    // Post-restart retarget attempts: activating A (the OLD active account)
    // and attaching A's signer must both leave the B-pinned intent silent.
    assert!(!core
        .handle(EngineMsg::SetActivePubkey(Some(active.public_key())))
        .iter()
        .any(|e| matches!(e, Effect::RequestSign(..))));
    assert!(!core
        .handle(EngineMsg::SignerAttached(active.public_key()))
        .iter()
        .any(|e| matches!(e, Effect::RequestSign(..))));

    // Only the exact override key's signer resumes the SAME receipt with
    // the frozen template.
    let (generation, template) = core
        .handle(EngineMsg::SignerAttached(override_keys.public_key()))
        .into_iter()
        .find_map(|e| match e {
            Effect::RequestSign(request_id, generation, u)
                if request_id == id && u.pubkey == override_keys.public_key() =>
            {
                Some((generation, u))
            }
            _ => None,
        })
        .expect("the override key's attach must re-arm the parked intent");
    let signed = template.sign_with_keys(&override_keys).unwrap();
    assert_eq!(signed.id, frozen_id, "the frozen body/id must be intact");
    let effects = core.handle(EngineMsg::SignerCompleted(id, generation, Ok(signed)));
    assert!(
        effects.iter().any(|e| matches!(
            e,
            Effect::EmitReceipt(rid, WriteStatus::Signed(event_id))
                if *rid == id && *event_id == frozen_id
        )),
        "completion must promote the original receipt to Signed as the override identity"
    );
    assert!(
        reattached.0.lock().unwrap().starts_with(&[
            WriteStatus::Accepted,
            WriteStatus::AwaitingCapability,
            WriteStatus::Signed(frozen_id)
        ]),
        "the reattached stream is the SAME receipt, extended in place \
         (routing facts may follow Signed) -- never a new one"
    );
    drop(core);
    let store = RedbStore::open(&path).unwrap();
    let rows = store.query(&nostr::Filter::new().id(frozen_id)).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].event.pubkey, override_keys.public_key());
    assert!(rows[0].event.verify().is_ok());
}

#[test]
fn exact_duplicate_coowners_recover_distinct_receipts_and_lossless_routes() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("coowners.redb");
    let keys = Keys::generate();
    let r1 = RelayUrl::parse("wss://one.example").unwrap();
    let r2 = RelayUrl::parse("wss://two.example").unwrap();
    let event = signed(&keys, "shared", 103);
    let s1 = signer_session(&r1, event.pubkey);
    let s2 = signer_session(&r2, event.pubkey);
    let (a, b) = {
        let store = RedbStore::open(&path).unwrap();
        let mut core = EngineCore::new(
            store,
            Box::new(
                FixtureDirectory::new()
                    .with_write(keys.public_key().to_hex(), [r1.clone(), r2.clone()]),
            ),
            10,
        );
        let h1 = RelayHandle {
            slot: 0,
            generation: 1,
        };
        let h2 = RelayHandle {
            slot: 1,
            generation: 1,
        };
        core.handle(EngineMsg::RelayConnected(h1, s1.clone()));
        core.handle(EngineMsg::RelayConnected(h2, s2.clone()));
        authenticate(&mut core, h1, &s1, &keys);
        authenticate(&mut core, h2, &s2, &keys);
        let publish = |core: &mut EngineCore<RedbStore>| {
            core.handle(EngineMsg::Publish(
                WriteIntent {
                    payload: WritePayload::Signed(event.clone()),
                    durability: Durability::Durable,
                    routing: WriteRouting::AuthorOutbox,
                    identity_override: None,
                },
                Box::new(Sink::default()),
            ))
        };
        let a = receipt_id(&publish(&mut core));
        let b = receipt_id(&publish(&mut core));
        assert_ne!(a, b);
        (a, b)
    };

    let store = RedbStore::open(&path).unwrap();
    let mut core = EngineCore::new(
        store,
        Box::new(
            FixtureDirectory::new()
                .with_write(keys.public_key().to_hex(), [r1.clone(), r2.clone()]),
        ),
        10,
    );
    let effects = core.recover_on_boot();
    assert_eq!(
        effects
            .iter()
            .filter(|effect| matches!(effect, Effect::PublishEvent(..)))
            .count(),
        0,
        "recovery queues connection work without allocating offline attempts"
    );
    let h1 = RelayHandle {
        slot: 0,
        generation: 1,
    };
    let h2 = RelayHandle {
        slot: 1,
        generation: 1,
    };
    core.handle(EngineMsg::RelayConnected(h1, s1.clone()));
    let mut replays = authenticate(&mut core, h1, &s1, &keys)
        .iter()
        .filter(
            |effect| matches!(effect, Effect::PublishEvent(_, replayed, _) if replayed == &event),
        )
        .count();
    core.handle(EngineMsg::RelayConnected(h2, s2.clone()));
    replays += authenticate(&mut core, h2, &s2, &keys)
        .iter()
        .filter(
            |effect| matches!(effect, Effect::PublishEvent(_, replayed, _) if replayed == &event),
        )
        .count();
    assert_eq!(
        replays, 2,
        "both relays make progress while the one-per-relay cap retains the other two lanes"
    );
    assert!(core
        .reattach_receipt(a, Box::new(Sink::default()))
        .is_attached());
    assert!(core
        .reattach_receipt(b, Box::new(Sink::default()))
        .is_attached());
}

#[test]
fn malformed_persisted_routing_fails_closed_without_dropping_the_obligation() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("malformed-route.redb");
    let keys = Keys::generate();
    let event = signed(&keys, "malformed", 104);
    let frozen = nostr::Event::new(
        event.id,
        event.pubkey,
        event.created_at,
        event.kind,
        event.tags.clone(),
        event.content.clone(),
        sentinel_signature(),
    );
    let (intent_id, receipt_id) = {
        let mut store = RedbStore::open(&path).unwrap();
        let outcome = store
            .accept_write(AcceptWrite {
                frozen,
                replaceable_base: None,
                expected_pubkey: keys.public_key(),
                signing_identity_ref: keys.public_key().to_hex(),
                durability: WriteDurability::Durable,
                routing: "future-routing-version-with-no-decoder".into(),
                sig_state: IntentSigState::Pending,
                accepted_at: Timestamp::from(104u64),
            })
            .unwrap();
        let intent_id = outcome.journaled_intent_id().unwrap();
        let receipt_id = ReceiptId(outcome.journaled_receipt_id().unwrap());
        (intent_id, receipt_id)
    };

    let store = RedbStore::open(&path).unwrap();
    let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 10);
    let effects = core.recover_on_boot();
    assert!(!effects
        .iter()
        .any(|effect| matches!(effect, Effect::PublishEvent(..))));
    let unreadable = Sink::default();
    assert_eq!(
        core.reattach_receipt(receipt_id, Box::new(unreadable.clone())),
        ReattachOutcome::RetainedButUnreadable
    );
    assert!(
        unreadable.0.lock().unwrap().is_empty(),
        "unreadable routing must replay no receipt prefix"
    );

    let sign_request = core.handle(EngineMsg::SignerAttached(keys.public_key()));
    let generation = sign_request
        .iter()
        .find_map(|effect| match effect {
            Effect::RequestSign(id, generation, unsigned) if *id == receipt_id => {
                assert_eq!(unsigned.pubkey, keys.public_key());
                Some(*generation)
            }
            _ => None,
        })
        .expect("the retained unsigned obligation must remain signer-owned");
    let completed = core.handle(EngineMsg::SignerCompleted(
        receipt_id,
        generation,
        Ok(event),
    ));
    assert!(!completed
        .iter()
        .any(|effect| matches!(effect, Effect::PublishEvent(..))));
    assert!(
        unreadable.0.lock().unwrap().is_empty(),
        "an unreadable reattach must not register for later signer facts"
    );
    let second = Sink::default();
    assert_eq!(
        core.reattach_receipt(receipt_id, Box::new(second.clone())),
        ReattachOutcome::RetainedButUnreadable
    );
    assert!(second.0.lock().unwrap().is_empty());
    drop(core);
    let store = RedbStore::open(&path).unwrap();
    assert!(store
        .recover_outbox()
        .iter()
        .any(|intent| intent.intent_id == intent_id));
    assert!(store.recover_attempts(intent_id).unwrap().is_empty());
}

#[test]
fn recovered_reserved_auth_write_is_quarantined_from_attempt_and_ok_correlation() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("quarantined-auth.redb");
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://quarantined-auth.example").unwrap();
    let signed = EventBuilder::auth("persisted collision", relay.clone())
        .custom_created_at(Timestamp::from(777))
        .sign_with_keys(&keys)
        .unwrap();
    let frozen = nostr::Event::new(
        signed.id,
        signed.pubkey,
        signed.created_at,
        signed.kind,
        signed.tags.clone(),
        signed.content.clone(),
        sentinel_signature(),
    );
    let receipt = {
        let mut store = RedbStore::open(&path).unwrap();
        let outcome = store
            .accept_write(AcceptWrite {
                frozen,
                replaceable_base: None,
                expected_pubkey: keys.public_key(),
                signing_identity_ref: keys.public_key().to_hex(),
                durability: WriteDurability::Durable,
                routing: "author-outbox".to_string(),
                sig_state: IntentSigState::Pending,
                accepted_at: Timestamp::from(777),
            })
            .unwrap();
        ReceiptId(outcome.journaled_receipt_id().unwrap())
    };

    let store = RedbStore::open(&path).unwrap();
    let mut core = EngineCore::new(
        store,
        Box::new(directory(keys.public_key(), relay.clone())),
        10,
    );
    let recovery = core.recover_on_boot();
    assert!(recovery.iter().any(|effect| matches!(
        effect,
        Effect::EmitReceipt(id, WriteStatus::Failed(reason))
            if *id == receipt && reason.contains("kind:22242") && reason.contains("quarantined")
    )));
    assert!(!recovery.iter().any(|effect| matches!(
        effect,
        Effect::EnsureRelay(_) | Effect::PublishEvent(..) | Effect::RequestSign(..)
    )));
    assert_eq!(
        core.reattach_receipt(receipt, Box::new(Sink::default())),
        ReattachOutcome::RetainedButUnreadable
    );

    let session = signer_session(&relay, keys.public_key());
    let handle = RelayHandle {
        slot: 4,
        generation: 1,
    };
    core.handle(EngineMsg::RelayConnected(handle, session.clone()));
    let stale_ok = core.handle(EngineMsg::RelayFrame(
        handle,
        session,
        RelayFrame::from(RelayMessage::ok(signed.id, true, "stale ordinary auth OK")),
    ));
    assert!(!stale_ok.iter().any(|effect| matches!(
        effect,
        Effect::EmitReceipt(_, WriteStatus::Acked(_))
            | Effect::PublishEvent(..)
            | Effect::RequestSign(..)
    )));
}

#[test]
fn corrupt_attempt_evidence_keeps_parent_obligation_and_boot_fails_closed() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("corrupt-boot.redb");
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://corrupt-boot.example").unwrap();
    let event = signed(&keys, "corrupt boot", 108);
    let session = signer_session(&relay, event.pubkey);
    let (intent_id, receipt_id) = {
        let store = RedbStore::open(&path).unwrap();
        let mut core = EngineCore::new(
            store,
            Box::new(directory(keys.public_key(), relay.clone())),
            10,
        );
        let handle = RelayHandle {
            slot: 0,
            generation: 1,
        };
        core.handle(EngineMsg::RelayConnected(handle, session.clone()));
        authenticate(&mut core, handle, &session, &keys);
        let effects = core.handle(EngineMsg::Publish(
            WriteIntent {
                payload: WritePayload::Signed(event),
                durability: Durability::Durable,
                routing: WriteRouting::AuthorOutbox,
                identity_override: None,
            },
            Box::new(Sink::default()),
        ));
        let receipt_id = receipt_id(&effects);
        drop(core);
        let store = RedbStore::open(&path).unwrap();
        (
            store
                .reattach_receipt(receipt_id.0)
                .unwrap()
                .unwrap()
                .intent_id
                .unwrap(),
            receipt_id,
        )
    };
    const ATTEMPTS: TableDefinition<&str, &str> = TableDefinition::new("outbox_attempts");
    let db = Database::open(&path).unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut table = tx.open_table(ATTEMPTS).unwrap();
        let key = format!(
            "{:020}:{:020}:{}:{:020}",
            intent_id.0,
            relay.as_str().len(),
            relay.as_str(),
            1
        );
        let json = table
            .get(key.as_str())
            .unwrap()
            .unwrap()
            .value()
            .to_string();
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        value["version"] = serde_json::json!(200);
        let encoded = serde_json::to_string(&value).unwrap();
        table.insert(key.as_str(), encoded.as_str()).unwrap();
    }
    tx.commit().unwrap();
    drop(db);

    let store = RedbStore::open(&path).unwrap();
    let mut core = EngineCore::new(store, Box::new(directory(keys.public_key(), relay)), 10);
    assert!(core.recover_on_boot().is_empty());
    let unreadable_sink = Sink::default();
    assert_eq!(
        core.reattach_receipt(receipt_id, Box::new(unreadable_sink.clone())),
        ReattachOutcome::RetainedButUnreadable
    );
    assert!(unreadable_sink.0.lock().unwrap().is_empty());
    drop(core);
    assert!(RedbStore::open(&path)
        .unwrap()
        .recover_outbox()
        .iter()
        .any(|intent| intent.intent_id == intent_id));
}

#[test]
fn retained_terminal_receipt_is_attached_and_replays_terminal_fact() {
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://terminal.example").unwrap();
    let store = nmp_store::MemoryStore::new();
    let mut core = EngineCore::new(store, Box::new(directory(keys.public_key(), relay)), 10);
    core.handle(EngineMsg::SetActivePubkey(Some(keys.public_key())));
    let effects = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(UnsignedEvent::new(
                keys.public_key(),
                Timestamp::from(500),
                Kind::TextNote,
                vec![],
                "terminal retained",
            )),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
        },
        Box::new(Sink::default()),
    ));
    let receipt = receipt_id(&effects);
    core.handle(EngineMsg::CancelWrite(receipt));

    let replay = Sink::default();
    assert_eq!(
        core.reattach_receipt(receipt, Box::new(replay.clone())),
        ReattachOutcome::Attached
    );
    assert_eq!(
        *replay.0.lock().unwrap(),
        vec![WriteStatus::Failed("write compensated".to_string())]
    );
}

#[test]
fn corrupt_retained_receipt_is_not_misreported_absent_and_keeps_obligation() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("corrupt-receipt.redb");
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://corrupt-receipt.example").unwrap();
    let (intent_id, receipt_id) = {
        let store = RedbStore::open(&path).unwrap();
        let mut core = EngineCore::new(store, Box::new(directory(keys.public_key(), relay)), 10);
        core.handle(EngineMsg::SetActivePubkey(Some(keys.public_key())));
        let effects = core.handle(EngineMsg::Publish(
            WriteIntent {
                payload: WritePayload::Unsigned(UnsignedEvent::new(
                    keys.public_key(),
                    Timestamp::from(501),
                    Kind::TextNote,
                    vec![],
                    "corrupt receipt",
                )),
                durability: Durability::Durable,
                routing: WriteRouting::AuthorOutbox,
                identity_override: None,
            },
            Box::new(Sink::default()),
        ));
        let receipt_id = receipt_id(&effects);
        drop(core);
        let store = RedbStore::open(&path).unwrap();
        let intent_id = store
            .reattach_receipt(receipt_id.0)
            .unwrap()
            .unwrap()
            .intent_id
            .unwrap();
        (intent_id, receipt_id)
    };

    const RECEIPTS: TableDefinition<&str, &str> = TableDefinition::new("outbox_receipts");
    let db = Database::open(&path).unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut table = tx.open_table(RECEIPTS).unwrap();
        table
            .insert(format!("{:020}", receipt_id.0).as_str(), "{")
            .unwrap();
    }
    tx.commit().unwrap();
    drop(db);

    let store = RedbStore::open(&path).unwrap();
    let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 10);
    assert!(core.recover_on_boot().is_empty());
    let replay = Sink::default();
    assert_eq!(
        core.reattach_receipt(receipt_id, Box::new(replay.clone())),
        ReattachOutcome::RetainedButUnreadable
    );
    assert!(replay.0.lock().unwrap().is_empty());
    drop(core);
    assert!(RedbStore::open(&path)
        .unwrap()
        .recover_outbox()
        .iter()
        .any(|intent| intent.intent_id == intent_id));
}

/// #115, cedar's flagged gap: the ruling text only said "resolve_routes
/// gains ONE arm," but a `PinnedHost`-routed pending write also has to
/// survive the reattach/restart path -- `routing_snapshot` (encode) is
/// wildcard-free so a missing arm there is a compile error, but
/// `parse_routing_snapshot` (decode) falls through to `None` on an
/// unrecognized prefix, which would silently mis-resolve a pinned-host
/// write on reboot without ever touching `resolve_routes` or a live relay
/// (invisible to `pinned_host_write.rs`'s falsifiers, which never cross a
/// restart boundary). This proves both snapshot arms actually exist and
/// round-trip: a `PinnedHost` write, restarted, still resolves to its
/// EXACT host, never any other relay, and never re-accepts.
#[test]
fn pinned_host_routing_round_trips_across_a_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("pinned-host.redb");
    let keys = Keys::generate();
    let host = RelayUrl::parse("wss://pinned-host.example").unwrap();
    let event = signed(&keys, "pinned", 600);
    let session = signer_session(&host, event.pubkey);

    let id = {
        let store = RedbStore::open(&path).unwrap();
        // Deliberately empty: `PinnedHost` routing must never consult the
        // directory (#115) -- if it ever did, this publish would have no
        // route to fall back on and this test would never see
        // `Effect::PublishEvent` at all.
        let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 10);
        let handle = RelayHandle {
            slot: 0,
            generation: 1,
        };
        core.handle(EngineMsg::RelayConnected(handle, session.clone()));
        authenticate(&mut core, handle, &session, &keys);
        let effects = core.handle(EngineMsg::Publish(
            WriteIntent {
                payload: WritePayload::Signed(event.clone()),
                durability: Durability::Durable,
                routing: WriteRouting::PinnedHost(HostAuthority::from_selected_host(host.clone())),
                identity_override: None,
            },
            Box::new(Sink::default()),
        ));
        assert!(effects.iter().any(|effect| matches!(effect,
            Effect::PublishEvent(r, e, _) if r == &session && e == &event
        )));
        receipt_id(&effects)
    };

    // Restart: drop the whole reducer/store, reopen the SAME file.
    let store = RedbStore::open(&path).unwrap();
    let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 10);
    let recovery = core.recover_on_boot();
    assert!(recovery
        .iter()
        .any(|effect| matches!(effect, Effect::EnsureRelay(r) if r == &session)));
    let handle = RelayHandle {
        slot: 0,
        generation: 1,
    };
    core.handle(EngineMsg::RelayConnected(handle, session.clone()));
    let recovery = authenticate(&mut core, handle, &session, &keys);
    assert!(
        recovery.iter().any(|effect| matches!(effect,
            Effect::PublishEvent(r, e, _) if r == &session && e == &event
        )),
        "a PinnedHost-routed pending write must still resolve to its exact host after a \
         restart -- parse_routing_snapshot must decode the persisted `pinned-host-hex:` \
         snapshot back into WriteRouting::PinnedHost, not just have routing_snapshot encode it"
    );
    assert!(
        !recovery
            .iter()
            .any(|effect| matches!(effect, Effect::EmitReceipt(_, WriteStatus::Accepted))),
        "boot recovery must not accept the write a second time"
    );

    let replay = Sink::default();
    assert!(core
        .reattach_receipt(id, Box::new(replay.clone()))
        .is_attached());
}

#[test]
fn corrupt_route_lane_evidence_is_unreadable_not_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("corrupt-route.redb");
    let keys = Keys::generate();
    let event = signed(&keys, "corrupt route", 502);
    let (intent_id, receipt_id) = {
        let store = RedbStore::open(&path).unwrap();
        let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 10);
        let effects = core.handle(EngineMsg::Publish(
            WriteIntent {
                payload: WritePayload::Signed(event),
                durability: Durability::Durable,
                routing: WriteRouting::AuthorOutbox,
                identity_override: None,
            },
            Box::new(Sink::default()),
        ));
        let receipt_id = receipt_id(&effects);
        drop(core);
        let store = RedbStore::open(&path).unwrap();
        let intent_id = store
            .reattach_receipt(receipt_id.0)
            .unwrap()
            .unwrap()
            .intent_id
            .unwrap();
        (intent_id, receipt_id)
    };

    const ROUTES: TableDefinition<&str, &str> = TableDefinition::new("outbox_route_revisions");
    let db = Database::open(&path).unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut table = tx.open_table(ROUTES).unwrap();
        table
            .insert(format!("{:020}:{:020}", intent_id.0, 1).as_str(), "{}")
            .unwrap();
    }
    tx.commit().unwrap();
    drop(db);

    let store = RedbStore::open(&path).unwrap();
    let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 10);
    assert!(core.recover_on_boot().is_empty());
    let replay = Sink::default();
    assert_eq!(
        core.reattach_receipt(receipt_id, Box::new(replay.clone())),
        ReattachOutcome::RetainedButUnreadable
    );
    assert!(replay.0.lock().unwrap().is_empty());
    drop(core);
    assert!(RedbStore::open(&path)
        .unwrap()
        .recover_outbox()
        .iter()
        .any(|intent| intent.intent_id == intent_id));
}
