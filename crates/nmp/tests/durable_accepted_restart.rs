//! Genuine redb close/reopen falsifiers for issues #2/#3 U4. No sleeps,
//! retry timers, or polling: restart is represented by dropping the whole
//! reducer/store and opening the database again.

use std::borrow::Cow;
use std::collections::BTreeSet;

use nmp::mechanism::core::{
    AuthCapability, AuthCapabilityInstance, AuthEffect, AuthPolicyOutcome, AuthSendCompletion,
    AuthSendOutcome, AuthSignerOutcome, Effect, EngineCore, EngineMsg, ReattachOutcome, ReceiptId,
};
use nmp::mechanism::publish_queue::{
    NotSentReason, RelayState, RelayWaiting, SigningState, WriteFact, WriteOutcome,
};
use nmp_grammar::{
    AccessContext, EventBuilder as NmpEventBuilder, Identity, RelaySessionKey, WriteIntent,
    WritePayload, WriteRouting,
};
use nmp_router::FixtureRoutingFacts;
use nmp_store::{
    sentinel_signature, testing, AcceptWrite, AcceptWritePayload, EventStore, IntentSigState,
    PublishQueueAttemptOutcome, PublishQueueTerminalOutcome, RedbStore, SigState,
};
use nmp_transport::{HandoffResult, RelayFrame, RelayHandle};
use nostr::{
    EventBuilder, Keys, Kind, PublicKey, RelayMessage, RelayUrl, Timestamp, UnsignedEvent,
};

fn receipt_id(effects: &[Effect]) -> ReceiptId {
    effects
        .iter()
        .find_map(|effect| match effect {
            Effect::WriteAccepted(id, _) => Some(*id),
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

fn directory(pk: PublicKey, relay: RelayUrl) -> FixtureRoutingFacts {
    FixtureRoutingFacts::new().with_outbound_routes(pk, [relay])
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
    core: &mut EngineCore,
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
            Effect::RelayAuth(AuthEffect::Send { token, event }) => {
                assert_eq!(token.epoch.session, *session);
                assert_eq!(token.epoch.handle, handle);
                Some((token, event))
            }
            _ => None,
        })
        .expect("signed AUTH requests exact-generation send");
    core.handle(EngineMsg::AuthSendCompleted(
        AuthSendCompletion::for_operation(&send_token, AuthSendOutcome::Accepted),
    ));
    core.handle(EngineMsg::RelayFrame(
        handle,
        session.clone(),
        RelayFrame::from(RelayMessage::ok(auth_event.id, true, "authenticated")),
    ))
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
        let mut core = EngineCore::new_with_fixture_routing_facts(
            store,
            directory(keys.public_key(), relay.clone()),
            10,
        );
        let handle = RelayHandle {
            slot: 0,
            generation: 1,
        };
        core.handle(EngineMsg::RelayConnected(handle, relay_session.clone()));
        authenticate(&mut core, handle, &relay_session, &keys);
        let effects = core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::Signed(event.clone()),
            routing: WriteRouting::Auto,
            identity: Identity::Active,
            correlation: None,
        }));
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

    let store = RedbStore::open(&path).unwrap();
    let mut core = EngineCore::new_with_fixture_routing_facts(
        store,
        FixtureRoutingFacts::new()
            .with_outbound_routes(keys.public_key(), [relay.clone(), appended.clone()]),
        10,
    );
    let recovery = core.recover_on_boot();
    assert!(
        recovery
            .iter()
            .any(|effect| matches!(effect, Effect::EnsureWriteRelay(r) if r == &relay_session))
            && recovery.iter().any(
                |effect| matches!(effect, Effect::EnsureWriteRelay(r) if r == &appended_session)
            ),
        "recovery preserves both lanes but allocates no attempt while offline"
    );
    assert!(
        !recovery
            .iter()
            .any(|effect| matches!(effect, Effect::WriteAccepted(..))),
        "boot recovery must not accept the write a second time"
    );

    let first = core.reattach_receipt(id);
    let second = core.reattach_receipt(id);
    assert!(first.is_attached());
    assert!(second.is_attached());
    assert_eq!(first.facts.len(), second.facts.len());
    assert!(
        !first
            .facts
            .iter()
            .any(|s| matches!(s, WriteFact::Relay { relay: r, state: RelayState::Sent { .. }, .. } if r == &relay)),
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
        Effect::EmitReceipt(receipt, WriteFact::Relay { relay: acked_relay, state: RelayState::Published, .. })
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
        vec![
            (1, &PublishQueueAttemptOutcome::Started),
            (2, &PublishQueueAttemptOutcome::Acked)
        ],
        "restart preserves the interrupted ordinal and ACKs a new retry ordinal"
    );
    let original_lane = store
        .recover_publish_queue_lanes(intent)
        .unwrap()
        .into_iter()
        .find(|lane| lane.key.relay == relay)
        .unwrap();
    assert_eq!(
        original_lane.state,
        nmp_store::PublishQueueLaneState::Terminal {
            ordinal: 2,
            outcome: PublishQueueTerminalOutcome::Acked,
        }
    );
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
        let mut core = EngineCore::new_with_fixture_routing_facts(
            store,
            directory(keys.public_key(), relay.clone()),
            10,
        );
        core.handle(EngineMsg::SetActivePubkey(Some(keys.public_key())));
        let effects = core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::Event(body_of(&unsigned)),
            routing: WriteRouting::Auto,
            identity: Identity::Active,
            correlation: None,
        }));
        receipt_id(&effects)
    };

    let store = RedbStore::open(&path).unwrap();
    let rows = store.query(&nostr::Filter::new().id(frozen_id)).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].provenance.local.as_ref().unwrap().sig_state,
        SigState::Pending
    );
    let mut core =
        EngineCore::new_with_fixture_routing_facts(store, directory(keys.public_key(), relay), 10);
    assert!(core.recover_on_boot().is_empty());
    let reattached = core.reattach_receipt(id);
    assert!(reattached.is_attached());
    assert_eq!(
        reattached.facts,
        vec![
            WriteFact::Signing(SigningState::AwaitingSigner {
                pubkey: keys.public_key()
            }),
            // Nobody is named, truthfully: this write is held on its
            // SIGNER, so no route lookup is outstanding for it to wait on.
            WriteFact::Destinations {
                relays: BTreeSet::new(),
                complete: false,
                awaiting_author_routes: BTreeSet::new(),
            },
        ]
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
/// an unsigned intent accepted under an explicit `Identity::Explicit(B)`
/// (named B while A was the current account, B's signer absent)
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
        let mut core = EngineCore::new_with_fixture_routing_facts(
            store,
            directory(override_keys.public_key(), relay.clone()),
            10,
        );
        // A is the current account; the override alone authorizes B's draft.
        core.handle(EngineMsg::SetActivePubkey(Some(active.public_key())));
        let effects = core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::Event(body_of(&unsigned)),
            routing: WriteRouting::Auto,
            identity: Identity::Explicit(override_keys.public_key()),
            correlation: None,
        }));
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
    let mut core = EngineCore::new_with_fixture_routing_facts(
        store,
        directory(override_keys.public_key(), relay),
        10,
    );
    assert!(core.recover_on_boot().is_empty());
    let reattached = core.reattach_receipt(id);
    assert!(reattached.is_attached());
    assert_eq!(
        reattached.facts,
        vec![
            WriteFact::Signing(SigningState::AwaitingSigner {
                pubkey: override_keys.public_key()
            }),
            WriteFact::Destinations {
                relays: BTreeSet::new(),
                complete: false,
                awaiting_author_routes: BTreeSet::new(),
            },
        ],
        "the parked pubkey must be the frozen override B, never the current account A; the \
         route park names nobody because an unsigned write has no route lookup outstanding"
    );

    // Post-restart retarget attempts: activating A (the OLD current account)
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
            Effect::EmitReceipt(rid, WriteFact::Signing(SigningState::Signed { event_id }))
                if *rid == id && *event_id == frozen_id
        )),
        "completion must promote the original receipt to Signed as the override identity"
    );
    let replay = core.reattach_receipt(id).facts;
    assert_eq!(
        replay.first(),
        Some(&WriteFact::Signing(SigningState::Signed {
            event_id: frozen_id
        }))
    );
    assert!(replay.iter().any(|status| matches!(
        status,
        WriteFact::Relay {
            state: RelayState::Waiting(RelayWaiting::NotConnected),
            ..
        }
    )));
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
        let mut core = EngineCore::new_with_fixture_routing_facts(
            store,
            FixtureRoutingFacts::new()
                .with_outbound_routes(keys.public_key(), [r1.clone(), r2.clone()]),
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
        let publish = |core: &mut EngineCore| {
            core.handle(EngineMsg::Publish(WriteIntent {
                payload: WritePayload::Signed(event.clone()),
                routing: WriteRouting::Auto,
                identity: Identity::Active,
                correlation: None,
            }))
        };
        let a = receipt_id(&publish(&mut core));
        let b = receipt_id(&publish(&mut core));
        assert_ne!(a, b);
        (a, b)
    };

    let store = RedbStore::open(&path).unwrap();
    let mut core = EngineCore::new_with_fixture_routing_facts(
        store,
        FixtureRoutingFacts::new()
            .with_outbound_routes(keys.public_key(), [r1.clone(), r2.clone()]),
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
    assert!(core.reattach_receipt(a).is_attached());
    assert!(core.reattach_receipt(b).is_attached());
}

fn assert_persisted_routing_fails_closed_without_dropping(
    database_name: &str,
    routing: String,
    route_probe: RelayUrl,
) {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join(database_name);
    let keys = Keys::generate();
    let event = signed(&keys, "unreadable routing", 104);
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
                payload: AcceptWritePayload::Event {
                    frozen: Box::new(frozen),
                    replaceable_base: None,
                    monotonic_stamp: false,
                    routing,
                    sig_state: IntentSigState::Pending,
                },
                expected_pubkey: keys.public_key(),
                signing_identity_ref: keys.public_key().to_hex(),
                accepted_at: Timestamp::from(104u64),
                correlation: None,
            })
            .unwrap();
        let intent_id = outcome.journaled_intent_id().unwrap();
        let receipt_id = ReceiptId(outcome.journaled_receipt_id().unwrap());
        (intent_id, receipt_id)
    };

    let store = RedbStore::open(&path).unwrap();
    let route_directory = directory(keys.public_key(), route_probe.clone());
    let mut core = EngineCore::new_with_fixture_routing_facts(store, route_directory, 10);
    let effects = core.recover_on_boot();
    assert!(!effects
        .iter()
        .any(|effect| matches!(effect, Effect::PublishEvent(..))));
    let unreadable = core.reattach_receipt(receipt_id);
    assert_eq!(unreadable.outcome, ReattachOutcome::RetainedButUnreadable);
    assert!(
        unreadable.facts.is_empty(),
        "unreadable routing must replay no receipt prefix"
    );

    // Keep the one relay this directory can offer both connected and
    // authenticated. The undecodable routing is therefore the ONLY reason
    // nothing reaches the wire: any decoder that resolved it -- or a silent
    // substitution of `auto`, the live default -- would make signer completion emit
    // `PublishEvent` and fail the no-wire assertion below.
    let route_session = signer_session(&route_probe, keys.public_key());
    let route_handle = RelayHandle {
        slot: 7,
        generation: 1,
    };
    core.handle(EngineMsg::RelayConnected(
        route_handle,
        route_session.clone(),
    ));
    authenticate(&mut core, route_handle, &route_session, &keys);

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
    let second = core.reattach_receipt(receipt_id);
    assert_eq!(second.outcome, ReattachOutcome::RetainedButUnreadable);
    assert!(second.facts.is_empty());
    drop(core);
    let store = RedbStore::open(&path).unwrap();
    assert!(store
        .recover_publish_queue()
        .expect("recover delivery")
        .iter()
        .any(|intent| intent.intent_id == intent_id));
    assert!(store.recover_attempts(intent_id).unwrap().is_empty());
}

#[test]
fn malformed_persisted_routing_fails_closed_without_dropping_the_obligation() {
    assert_persisted_routing_fails_closed_without_dropping(
        "malformed-route.redb",
        "future-routing-version-with-no-decoder".into(),
        RelayUrl::parse("wss://malformed-route-probe.example").unwrap(),
    );
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
                payload: AcceptWritePayload::Event {
                    frozen: Box::new(frozen),
                    replaceable_base: None,
                    monotonic_stamp: false,
                    routing: "auto".to_string(),
                    sig_state: IntentSigState::Pending,
                },
                expected_pubkey: keys.public_key(),
                signing_identity_ref: keys.public_key().to_hex(),
                accepted_at: Timestamp::from(777),
                correlation: None,
            })
            .unwrap();
        ReceiptId(outcome.journaled_receipt_id().unwrap())
    };

    let store = RedbStore::open(&path).unwrap();
    let mut core = EngineCore::new_with_fixture_routing_facts(
        store,
        directory(keys.public_key(), relay.clone()),
        10,
    );
    let recovery = core.recover_on_boot();
    assert!(recovery.iter().any(|effect| matches!(
        effect,
        Effect::EmitReceipt(id, WriteFact::Signing(SigningState::Refused { reason }))
            if *id == receipt && reason.contains("kind:22242") && reason.contains("quarantined")
    )));
    assert!(!recovery.iter().any(|effect| matches!(
        effect,
        Effect::EnsureReadRelay(_)
            | Effect::EnsureWriteRelay(_)
            | Effect::PublishEvent(..)
            | Effect::RequestSign(..)
    )));
    assert_eq!(
        core.reattach_receipt(receipt).outcome,
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
        Effect::EmitReceipt(
            _,
            WriteFact::Relay {
                relay: _,
                state: RelayState::Published,
                ..
            }
        ) | Effect::PublishEvent(..)
            | Effect::RequestSign(..)
    )));

    let (cancelled, cancellation) = core.cancel_write(receipt);
    assert_eq!(
        cancelled,
        Ok(nmp::mechanism::publish_queue::CancelWriteOutcome::Cancelled)
    );
    assert!(cancellation.iter().any(
        |effect| matches!(effect, Effect::EmitReceipt(id, WriteFact::Outcome(WriteOutcome::NotSent(NotSentReason::Cancelled))) if *id == receipt)
    ));
    assert!(!cancellation.iter().any(|effect| matches!(
        effect,
        Effect::EnsureReadRelay(_)
            | Effect::EnsureWriteRelay(_)
            | Effect::PublishEvent(..)
            | Effect::RequestSign(..)
    )));

    drop(core);
    let store = RedbStore::open(&path).unwrap();
    assert!(store
        .recover_publish_queue()
        .expect("recover delivery")
        .is_empty());
    let mut reopened =
        EngineCore::new_with_fixture_routing_facts(store, directory(keys.public_key(), relay), 10);
    assert!(reopened.recover_on_boot().is_empty());
    let replay = reopened.reattach_receipt(receipt);
    assert!(replay.is_attached());
    assert_eq!(
        replay.facts,
        vec![WriteFact::Outcome(WriteOutcome::NotSent(
            NotSentReason::Cancelled
        ))]
    );
}

#[test]
fn signed_receipt_replays_signed_and_refuses_cancellation_after_reopen() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("signed-replay.redb");
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://signed-replay.example").unwrap();
    let event = signed(&keys, "already signed", 778);
    let receipt = {
        let store = RedbStore::open(&path).unwrap();
        let mut core = EngineCore::new_with_fixture_routing_facts(
            store,
            directory(keys.public_key(), relay.clone()),
            10,
        );
        core.handle(EngineMsg::SetActivePubkey(Some(keys.public_key())));
        let effects = core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::Signed(event.clone()),
            routing: WriteRouting::Auto,
            identity: Identity::Active,
            correlation: None,
        }));
        assert!(effects.iter().any(
            |effect| matches!(effect, Effect::EmitReceipt(_, WriteFact::Signing(SigningState::Signed { event_id: id })) if *id == event.id)
        ));
        receipt_id(&effects)
    };

    let store = RedbStore::open(&path).unwrap();
    let mut reopened =
        EngineCore::new_with_fixture_routing_facts(store, directory(keys.public_key(), relay), 10);
    reopened.recover_on_boot();
    let replay = reopened.reattach_receipt(receipt);
    assert!(replay.is_attached());
    assert!(
        replay
            .facts
            .contains(&WriteFact::Signing(SigningState::Signed {
                event_id: event.id
            })),
        "the signature is durable, so a restart replays it: {:?}",
        replay.facts
    );

    let (refused, effects) = reopened.cancel_write(receipt);
    assert!(matches!(
        refused,
        Err(nmp::mechanism::publish_queue::CancelWriteError::AlreadySigned {
            receipt_id,
            event_id,
        }) if receipt_id == receipt && event_id == event.id
    ));
    assert!(effects.is_empty());
    assert!(reopened
        .reattach_receipt(receipt)
        .facts
        .contains(&WriteFact::Signing(SigningState::Signed {
            event_id: event.id
        })));
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
        let mut core = EngineCore::new_with_fixture_routing_facts(
            store,
            directory(keys.public_key(), relay.clone()),
            10,
        );
        let handle = RelayHandle {
            slot: 0,
            generation: 1,
        };
        core.handle(EngineMsg::RelayConnected(handle, session.clone()));
        authenticate(&mut core, handle, &session, &keys);
        let effects = core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::Signed(event),
            routing: WriteRouting::Auto,
            identity: Identity::Active,
            correlation: None,
        }));
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
    testing::corrupt_first_publish_queue_attempt(&path, &intent_id.0.to_be_bytes())
        .expect("store-owned attempt corruption");

    let store = RedbStore::open(&path).unwrap();
    let mut core =
        EngineCore::new_with_fixture_routing_facts(store, directory(keys.public_key(), relay), 10);
    assert!(core.recover_on_boot().is_empty());
    let unreadable = core.reattach_receipt(receipt_id);
    assert_eq!(unreadable.outcome, ReattachOutcome::RetainedButUnreadable);
    assert!(unreadable.facts.is_empty());
    drop(core);
    assert!(RedbStore::open(&path)
        .unwrap()
        .recover_publish_queue()
        .expect("recover delivery")
        .iter()
        .any(|intent| intent.intent_id == intent_id));
}

#[test]
fn retained_terminal_receipt_is_attached_and_replays_terminal_fact() {
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://terminal.example").unwrap();
    let store = nmp_store::RedbStore::temporary().expect("temporary Redb store");
    let mut core =
        EngineCore::new_with_fixture_routing_facts(store, directory(keys.public_key(), relay), 10);
    core.handle(EngineMsg::SetActivePubkey(Some(keys.public_key())));
    let effects = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(NmpEventBuilder {
            kind: Kind::TextNote,
            tags: (vec![]).into_iter().collect(),
            content: ("terminal retained").into(),
            created_at: Some(Timestamp::from(500)),
        }),
        routing: WriteRouting::Auto,
        identity: Identity::Active,
        correlation: None,
    }));
    let receipt = receipt_id(&effects);
    core.handle(EngineMsg::CancelWrite(receipt));

    let replay = core.reattach_receipt(receipt);
    assert_eq!(replay.outcome, ReattachOutcome::Attached);
    assert_eq!(
        replay.facts,
        vec![WriteFact::Outcome(WriteOutcome::NotSent(
            NotSentReason::Cancelled
        ))]
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
        let mut core = EngineCore::new_with_fixture_routing_facts(
            store,
            directory(keys.public_key(), relay),
            10,
        );
        core.handle(EngineMsg::SetActivePubkey(Some(keys.public_key())));
        let effects = core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::Event(NmpEventBuilder {
                kind: Kind::TextNote,
                tags: (vec![]).into_iter().collect(),
                content: ("corrupt receipt").into(),
                created_at: Some(Timestamp::from(501)),
            }),
            routing: WriteRouting::Auto,
            identity: Identity::Active,
            correlation: None,
        }));
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

    testing::corrupt_publish_queue_receipt(&path, receipt_id.0)
        .expect("store-owned receipt corruption");

    let store = RedbStore::open(&path).unwrap();
    let mut core = EngineCore::new(store, 10);
    assert!(core.recover_on_boot().is_empty());
    let replay = core.reattach_receipt(receipt_id);
    assert_eq!(replay.outcome, ReattachOutcome::RetainedButUnreadable);
    assert!(replay.facts.is_empty());

    let (refused, effects) = core.cancel_write(receipt_id);
    assert!(matches!(
        refused,
        Err(nmp::mechanism::publish_queue::CancelWriteError::PersistenceFailed {
            receipt_id: failed_id,
            reason,
        }) if failed_id == receipt_id && reason.contains("decode publish queue receipt")
    ));
    assert!(
        effects.is_empty(),
        "corruption must not fabricate cancellation"
    );

    drop(core);
    let store = RedbStore::open(&path).unwrap();
    let recovered = store
        .recover_publish_queue()
        .expect("recover delivery")
        .into_iter()
        .find(|intent| intent.intent_id == intent_id)
        .expect("failed cancellation must retain open work");
    let (recovered_event, _, _, _) = recovered.event_work().expect("ordinary event work");
    assert_eq!(
        store
            .query(&nostr::Filter::new().id(recovered_event.id))
            .unwrap()
            .len(),
        1,
        "the cancellation transaction must roll back its pending-row retraction"
    );
}

/// #719: the first kind:10002's exact bootstrap relay set is part of the
/// durable obligation. A process may die after acceptance but before either
/// relay ACKs; recovery must reopen exactly that set without consulting or
/// mutating the author directory.
#[test]
fn relay_list_bootstrap_routing_round_trips_across_a_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("nip65-bootstrap.redb");
    let keys = Keys::generate();
    let relay_a = RelayUrl::parse("wss://bootstrap-a.example").unwrap();
    let relay_b = RelayUrl::parse("wss://bootstrap-b.example").unwrap();
    let event = signed(&keys, "bootstrap", 601);
    let session_a = signer_session(&relay_a, event.pubkey);
    let session_b = signer_session(&relay_b, event.pubkey);

    let id = {
        let store = RedbStore::open(&path).unwrap();
        let mut core = EngineCore::new(store, 10);
        let effects = core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::Signed(event),
            routing: WriteRouting::Explicit(vec![relay_b.clone(), relay_a.clone()]),
            identity: Identity::Active,
            correlation: None,
        }));
        let ensured = effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::EnsureWriteRelay(session) => Some(session.clone()),
                _ => None,
            })
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            ensured,
            std::collections::BTreeSet::from([session_a.clone(), session_b.clone()])
        );
        receipt_id(&effects)
    };

    let store = RedbStore::open(&path).unwrap();
    let mut core = EngineCore::new(store, 10);
    let recovery = core.recover_on_boot();
    let ensured = recovery
        .iter()
        .filter_map(|effect| match effect {
            Effect::EnsureWriteRelay(session) => Some(session.clone()),
            _ => None,
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        ensured,
        std::collections::BTreeSet::from([session_a, session_b]),
        "restart must recover the exact relay set without consulting the directory"
    );
    assert!(
        !recovery
            .iter()
            .any(|effect| matches!(effect, Effect::WriteAccepted(..))),
        "boot recovery must not accept the bootstrap a second time"
    );
    assert!(core.reattach_receipt(id).is_attached());
}

#[test]
fn corrupt_route_lane_evidence_is_unreadable_not_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("corrupt-route.redb");
    let keys = Keys::generate();
    let event = signed(&keys, "corrupt route", 502);
    let (intent_id, receipt_id) = {
        let store = RedbStore::open(&path).unwrap();
        let mut core = EngineCore::new(store, 10);
        let effects = core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::Signed(event),
            routing: WriteRouting::Auto,
            identity: Identity::Active,
            correlation: None,
        }));
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

    let mut route_key = [0_u8; 16];
    route_key[..8].copy_from_slice(&intent_id.0.to_be_bytes());
    route_key[8..].copy_from_slice(&1u64.to_be_bytes());
    testing::insert_corrupt_publish_queue_route_revision(&path, &route_key)
        .expect("store-owned route-revision corruption");

    let store = RedbStore::open(&path).unwrap();
    let mut core = EngineCore::new(store, 10);
    assert!(core.recover_on_boot().is_empty());
    let replay = core.reattach_receipt(receipt_id);
    assert_eq!(replay.outcome, ReattachOutcome::RetainedButUnreadable);
    assert!(replay.facts.is_empty());
    drop(core);
    assert!(RedbStore::open(&path)
        .unwrap()
        .recover_publish_queue()
        .expect("recover delivery")
        .iter()
        .any(|intent| intent.intent_id == intent_id));
}

/// #790 boot falsifier: an unreadable durable journal degrades explicitly
/// instead of panicking the host or silently booting as "nothing open".
///
/// Before #790 `recover_publish_queue` returned a bare `Vec` and `.expect()`ed the
/// row decode, so this exact file aborted the process inside
/// `recover_on_boot` — the one moment an embedding app is least able to
/// survive it. The contract now: exactly one #122 degradation effect, and
/// not one fabricated fact. No receipt, no lane wake, no signer request, no
/// publish. A partial prefix of the journal is not a safe answer either, so
/// the corrupt row fails the whole call rather than shortening the
/// obligation set.
#[test]
fn boot_degrades_explicitly_when_the_durable_journal_will_not_decode() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("unreadable-journal.redb");
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://unreadable-journal.example").unwrap();
    let event = signed(&keys, "unreadable journal", 991);
    {
        let mut store = RedbStore::open(&path).unwrap();
        store
            .accept_write(AcceptWrite {
                payload: AcceptWritePayload::Event {
                    frozen: Box::new(nostr::Event::new(
                        event.id,
                        event.pubkey,
                        event.created_at,
                        event.kind,
                        event.tags.clone(),
                        event.content.clone(),
                        sentinel_signature(),
                    )),
                    replaceable_base: None,
                    monotonic_stamp: false,
                    routing: "auto".to_string(),
                    sig_state: IntentSigState::Pending,
                },
                expected_pubkey: keys.public_key(),
                signing_identity_ref: "local".to_string(),
                accepted_at: Timestamp::from(991),
                correlation: None,
            })
            .expect("accept_write");
    }

    // Corrupt the one journal row through a raw handle; no store door can
    // write these bytes, which is exactly why this class needs a falsifier.
    testing::corrupt_first_publish_queue_intent(&path).expect("store-owned intent corruption");

    let store = RedbStore::open(&path).unwrap();
    assert!(
        store.recover_publish_queue().is_err(),
        "an undecodable journal row is an error, never an empty recovery"
    );
    let mut core = EngineCore::new_with_fixture_routing_facts(
        store,
        directory(keys.public_key(), relay.clone()),
        10,
    );
    let effects = core.recover_on_boot();

    let degradations: Vec<_> = effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::EmitDiagnostics(snapshot) => snapshot.store_degraded.clone(),
            _ => None,
        })
        .collect();
    assert_eq!(
        degradations.len(),
        1,
        "boot degrades exactly once: {effects:?}"
    );
    assert!(
        degradations[0].contains("decode publish queue intent"),
        "the degradation names the unreadable row: {}",
        degradations[0]
    );
    assert!(
        !effects.iter().any(|effect| matches!(
            effect,
            Effect::EmitReceipt(..)
                | Effect::EnsureWriteRelay(_)
                | Effect::EnsureReadRelay(_)
                | Effect::PublishEvent(..)
                | Effect::RequestSign(..)
        )),
        "no receipt, lane wake, publish, or signer request may be fabricated \
         from a journal that could not be read: {effects:?}"
    );
}

/// The same body these fixtures already build, said the way an app says it:
/// a builder states the kind, the tags, the content and (here, so the
/// assertions can name exact ids) the timestamp. The author is not part of
/// it -- the write's identity decides that at acceptance.
fn body_of(unsigned: &nostr::UnsignedEvent) -> nmp_grammar::EventBuilder {
    nmp_grammar::EventBuilder {
        kind: unsigned.kind,
        tags: unsigned.tags.iter().cloned().collect(),
        content: unsigned.content.clone(),
        created_at: Some(unsigned.created_at),
    }
}
