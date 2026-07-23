use std::borrow::Cow;
use std::collections::BTreeSet;

use nmp_grammar::{AccessContext, ConcreteFilter, ContextualAtom, SourceAuthority};
use nmp_router::FixtureDirectory;
use nmp_store::MemoryStore;
use nmp_transport::{DisconnectReason, RelayFrame, RelayHandle};
use nostr::{EventBuilder, EventId, Keys, Kind, RelayMessage, RelayUrl, SubscriptionId, Timestamp};

use super::*;

const POLICY: AuthCapabilityInstance = AuthCapabilityInstance(41);
const SIGNER: AuthCapabilityInstance = AuthCapabilityInstance(42);

struct DiscardReceipt;

impl ReceiptSink for DiscardReceipt {
    fn on_status(&self, _: WriteStatus) -> bool {
        true
    }
}

struct Fixture {
    core: EngineCore<MemoryStore>,
    keys: Keys,
    session: RelaySessionKey,
    handle: RelayHandle,
    sub_id: SubId,
}

impl Fixture {
    fn new() -> Self {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://auth-core.example.com").unwrap();
        let session = RelaySessionKey::new(relay.clone(), AccessContext::Nip42(keys.public_key()));
        let filter = ConcreteFilter {
            kinds: Some(BTreeSet::from([1])),
            ..ConcreteFilter::default()
        };
        let atom = ContextualAtom {
            filter,
            source: SourceAuthority::Pinned(BTreeSet::from([relay.clone()])),
            access: session.access,
            routing_evidence: BTreeSet::new(),
        };
        let directory = FixtureDirectory::new().with_write(keys.public_key().to_hex(), [relay]);
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(directory), 10);
        core.attribution.observe_demand([&atom]);
        core.router
            .compile(&BTreeSet::from([atom]), core.directory.as_ref(), core.cap);
        let sub_id = core.router.plan().reqs[&session][0].sub_id.clone();
        let handle = RelayHandle {
            slot: 7,
            generation: 3,
        };
        core.handle(EngineMsg::RelayConnected(handle, session.clone()));
        Self {
            core,
            keys,
            session,
            handle,
            sub_id,
        }
    }

    fn challenge(&mut self, challenge: &str) -> (Vec<Effect>, Option<AuthOpToken>) {
        let effects = self.core.handle(EngineMsg::RelayFrame(
            self.handle,
            self.session.clone(),
            RelayFrame::from(RelayMessage::Auth {
                challenge: Cow::Owned(challenge.to_string()),
            }),
        ));
        let token = effects.iter().find_map(|effect| match effect {
            Effect::RelayAuth(AuthEffect::RequestPolicy { token, .. }) => Some(token.clone()),
            _ => None,
        });
        (effects, token)
    }

    fn allow(&mut self, token: AuthOpToken) -> (AuthOpToken, nostr::UnsignedEvent) {
        self.core.handle(EngineMsg::AuthCapabilityBound {
            token: token.clone(),
            capability: AuthCapability::Policy,
            instance: POLICY,
        });
        let effects = self.core.handle(EngineMsg::AuthPolicyCompleted(
            token,
            Some(POLICY),
            AuthPolicyOutcome::Allow,
        ));
        effects
            .into_iter()
            .find_map(|effect| match effect {
                Effect::RelayAuth(AuthEffect::RequestSignature { token, unsigned }) => {
                    Some((token, *unsigned))
                }
                _ => None,
            })
            .expect("allow requests the frozen AUTH template")
    }

    fn sign(
        &mut self,
        token: AuthOpToken,
        unsigned: nostr::UnsignedEvent,
    ) -> (AuthOpToken, nostr::Event) {
        self.core.handle(EngineMsg::AuthCapabilityBound {
            token: token.clone(),
            capability: AuthCapability::Signer,
            instance: SIGNER,
        });
        let signed = unsigned.sign_with_keys(&self.keys).unwrap();
        let effects = self.core.handle(EngineMsg::AuthSignerCompleted(
            token,
            Some(SIGNER),
            AuthSignerOutcome::Signed(signed.clone()),
        ));
        effects
            .into_iter()
            .find_map(|effect| match effect {
                Effect::RelayAuth(AuthEffect::Send {
                    token,
                    epoch,
                    event,
                }) => {
                    assert_eq!(epoch.session, self.session);
                    assert_eq!(epoch.handle, self.handle);
                    Some((token, *event))
                }
                _ => None,
            })
            .expect("valid signer completion requests exact-generation AUTH send")
    }

    fn send_accepted(&mut self, token: AuthOpToken) {
        self.core.handle(EngineMsg::AuthSendCompleted(
            token,
            AuthSendOutcome::Accepted,
        ));
    }

    fn ok(&mut self, event_id: EventId, status: bool) -> Vec<Effect> {
        self.core.handle(EngineMsg::RelayFrame(
            self.handle,
            self.session.clone(),
            RelayFrame::from(RelayMessage::ok(event_id, status, "auth result")),
        ))
    }
}

fn auth_phase(fixture: &Fixture) -> &AuthSessionPhase {
    &fixture.core.auth_sessions[&fixture.session].phase
}

fn bind(
    fixture: &mut Fixture,
    token: &AuthOpToken,
    capability: AuthCapability,
    instance: AuthCapabilityInstance,
) {
    fixture.core.handle(EngineMsg::AuthCapabilityBound {
        token: token.clone(),
        capability,
        instance,
    });
}

#[test]
fn exact_success_replays_once_and_only_then_allows_eose_credit() {
    let mut fixture = Fixture::new();
    let (_, policy) = fixture.challenge("challenge-a");
    let (sign_token, unsigned) = fixture.allow(policy.unwrap());
    assert_eq!(unsigned.kind, Kind::Authentication);
    assert_eq!(unsigned.pubkey, fixture.keys.public_key());
    assert_eq!(unsigned.created_at, Timestamp::from(0));
    let (send_token, event) = fixture.sign(sign_token, unsigned);

    let premature = fixture.core.handle(EngineMsg::RelayFrame(
        fixture.handle,
        fixture.session.clone(),
        RelayFrame::from(RelayMessage::eose(SubscriptionId::new(wire_sub_id_string(
            &fixture.sub_id,
        )))),
    ));
    assert!(!premature
        .iter()
        .any(|effect| matches!(effect, Effect::RecordCoverage(..))));

    fixture.send_accepted(send_token);
    assert!(matches!(
        auth_phase(&fixture),
        AuthSessionPhase::AwaitingOk { .. }
    ));
    let ready = fixture.ok(event.id, true);
    assert_eq!(
        ready
            .iter()
            .filter(
                |effect| matches!(effect, Effect::Replay(session, _) if session == &fixture.session)
            )
            .count(),
        1
    );
    assert!(fixture
        .core
        .auth_ready_sessions
        .contains_key(&fixture.session));
    assert!(matches!(
        auth_phase(&fixture),
        AuthSessionPhase::Ready { .. }
    ));
    assert!(
        fixture.ok(event.id, true).is_empty(),
        "duplicate OK is a no-op"
    );

    let credited = fixture.core.handle(EngineMsg::RelayFrame(
        fixture.handle,
        fixture.session.clone(),
        RelayFrame::from(RelayMessage::eose(SubscriptionId::new(wire_sub_id_string(
            &fixture.sub_id,
        )))),
    ));
    assert!(credited
        .iter()
        .any(|effect| matches!(effect, Effect::RecordCoverage(..))));
}

#[test]
fn exact_early_ok_waits_for_successful_handoff_and_failed_handoff_never_readies() {
    let mut fixture = Fixture::new();
    let (_, policy) = fixture.challenge("early-ok");
    let (sign_token, unsigned) = fixture.allow(policy.unwrap());
    let (send_token, event) = fixture.sign(sign_token, unsigned);

    assert!(fixture.ok(event.id, true).is_empty());
    assert!(!fixture
        .core
        .auth_ready_sessions
        .contains_key(&fixture.session));
    let ready = fixture.core.handle(EngineMsg::AuthSendCompleted(
        send_token,
        AuthSendOutcome::Accepted,
    ));
    assert_eq!(
        ready
            .iter()
            .filter(|effect| matches!(effect, Effect::Replay(..)))
            .count(),
        1
    );

    let (_, policy) = fixture.challenge("failed-handoff");
    let (sign_token, unsigned) = fixture.allow(policy.unwrap());
    let (send_token, event) = fixture.sign(sign_token, unsigned);
    assert!(fixture.ok(event.id, true).is_empty());
    fixture.core.handle(EngineMsg::AuthSendCompleted(
        send_token,
        AuthSendOutcome::Unavailable,
    ));
    assert!(!fixture
        .core
        .auth_ready_sessions
        .contains_key(&fixture.session));
    assert!(matches!(auth_phase(&fixture), AuthSessionPhase::Error));
}

#[test]
fn every_challenge_supersedes_and_identical_challenges_mint_distinct_events() {
    let mut fixture = Fixture::new();
    let (_, first_policy) = fixture.challenge("same");
    let first_policy = first_policy.unwrap();
    let (first_sign, first_unsigned) = fixture.allow(first_policy.clone());
    let first_created_at = first_unsigned.created_at;
    let (first_send, first_event) = fixture.sign(first_sign.clone(), first_unsigned);

    let (supersede, second_policy) = fixture.challenge("same");
    let second_policy = second_policy.unwrap();
    assert!(matches!(
        supersede.first(),
        Some(Effect::RelayAuth(AuthEffect::Cancel(epoch))) if epoch == &first_policy.epoch
    ));
    assert!(second_policy.epoch.sequence > first_policy.epoch.sequence);
    assert!(second_policy.sequence > first_send.sequence);
    assert!(fixture
        .core
        .handle(EngineMsg::AuthSendCompleted(
            first_send,
            AuthSendOutcome::Accepted,
        ))
        .is_empty());
    assert!(fixture
        .core
        .handle(EngineMsg::AuthSignerCompleted(
            first_sign,
            Some(SIGNER),
            AuthSignerOutcome::Unavailable,
        ))
        .is_empty());
    assert!(fixture.ok(first_event.id, true).is_empty());

    let (second_sign, second_unsigned) = fixture.allow(second_policy);
    assert_eq!(
        second_unsigned.created_at.as_secs(),
        first_created_at.as_secs() + 1
    );
    let (second_send, second_event) = fixture.sign(second_sign, second_unsigned);
    assert_ne!(first_event.id, second_event.id);
    fixture.send_accepted(second_send);
    assert_eq!(
        fixture
            .ok(second_event.id, true)
            .iter()
            .filter(|effect| matches!(effect, Effect::Replay(..)))
            .count(),
        1
    );
}

#[test]
fn empty_unavailable_denied_and_invalid_signer_results_are_truthful() {
    let mut fixture = Fixture::new();
    let (empty, token) = fixture.challenge("");
    assert!(token.is_none());
    assert!(!empty
        .iter()
        .any(|effect| matches!(effect, Effect::RelayAuth(AuthEffect::RequestPolicy { .. }))));
    assert!(matches!(auth_phase(&fixture), AuthSessionPhase::Error));

    let (_, policy) = fixture.challenge("unavailable");
    fixture.core.handle(EngineMsg::AuthPolicyCompleted(
        policy.unwrap(),
        None,
        AuthPolicyOutcome::Unavailable,
    ));
    assert!(matches!(auth_phase(&fixture), AuthSessionPhase::Error));

    let (_, policy) = fixture.challenge("denied");
    let policy = policy.unwrap();
    fixture.core.handle(EngineMsg::AuthCapabilityBound {
        token: policy.clone(),
        capability: AuthCapability::Policy,
        instance: POLICY,
    });
    fixture.core.handle(EngineMsg::AuthPolicyCompleted(
        policy,
        Some(POLICY),
        AuthPolicyOutcome::Deny {
            reason: "operator denied".to_string(),
        },
    ));
    assert!(matches!(auth_phase(&fixture), AuthSessionPhase::Denied));

    let (_, policy) = fixture.challenge("bad signer");
    let (sign_token, unsigned) = fixture.allow(policy.unwrap());
    fixture.core.handle(EngineMsg::AuthCapabilityBound {
        token: sign_token.clone(),
        capability: AuthCapability::Signer,
        instance: SIGNER,
    });
    let wrong = unsigned.sign_with_keys(&Keys::generate()).unwrap();
    assert!(fixture
        .core
        .handle(EngineMsg::AuthSignerCompleted(
            sign_token,
            Some(SIGNER),
            AuthSignerOutcome::Signed(wrong),
        ))
        .is_empty());
    assert!(matches!(auth_phase(&fixture), AuthSessionPhase::Error));

    let (_, policy) = fixture.challenge("rejected signer");
    let (sign_token, _) = fixture.allow(policy.unwrap());
    fixture.core.handle(EngineMsg::AuthCapabilityBound {
        token: sign_token.clone(),
        capability: AuthCapability::Signer,
        instance: SIGNER,
    });
    fixture.core.handle(EngineMsg::AuthSignerCompleted(
        sign_token,
        Some(SIGNER),
        AuthSignerOutcome::Rejected {
            reason: "user rejected".to_string(),
        },
    ));
    assert!(matches!(auth_phase(&fixture), AuthSessionPhase::Denied));
}

#[test]
fn capability_identity_and_teardown_invalidate_only_the_exact_live_epoch() {
    let mut fixture = Fixture::new();
    let (_, policy) = fixture.challenge("capability");
    let (sign_token, unsigned) = fixture.allow(policy.unwrap());
    let (send_token, event) = fixture.sign(sign_token, unsigned);
    fixture.send_accepted(send_token);
    fixture.ok(event.id, true);

    assert!(fixture
        .core
        .handle(EngineMsg::AuthCapabilityInvalidated(
            fixture.keys.public_key(),
            AuthCapability::Signer,
            AuthCapabilityInstance(999),
        ))
        .is_empty());
    assert!(matches!(
        auth_phase(&fixture),
        AuthSessionPhase::Ready { .. }
    ));

    let invalidated = fixture.core.handle(EngineMsg::AuthCapabilityInvalidated(
        fixture.keys.public_key(),
        AuthCapability::Signer,
        SIGNER,
    ));
    assert!(invalidated
        .iter()
        .any(|effect| matches!(effect, Effect::RelayAuth(AuthEffect::Cancel(_)))));
    assert!(invalidated
        .iter()
        .any(|effect| matches!(effect, Effect::Wire(_))));
    assert!(!fixture
        .core
        .auth_ready_sessions
        .contains_key(&fixture.session));
    assert!(matches!(auth_phase(&fixture), AuthSessionPhase::Error));

    let epoch = fixture.core.auth_sessions[&fixture.session].epoch.clone();
    let disconnected = fixture.core.handle(EngineMsg::RelayDisconnected(
        fixture.handle,
        fixture.session.clone(),
        DisconnectReason::Closed,
    ));
    assert!(disconnected.iter().any(|effect| matches!(
        effect,
        Effect::RelayAuth(AuthEffect::Cancel(cancelled)) if cancelled == &epoch
    )));
    assert!(fixture.core.auth_sessions.is_empty());
    assert!(fixture.core.auth_ready_sessions.is_empty());
}

#[test]
fn auth_state_stays_one_entry_per_session_under_churn_and_kind_is_reserved() {
    let mut fixture = Fixture::new();
    let mut last_epoch = 0;
    let mut last_operation = 0;
    for ordinal in 0..512 {
        let (_, token) = fixture.challenge(&format!("challenge-{ordinal}"));
        let token = token.unwrap();
        assert!(token.epoch.sequence > last_epoch);
        assert!(token.sequence > last_operation);
        last_epoch = token.epoch.sequence;
        last_operation = token.sequence;
        assert_eq!(fixture.core.auth_sessions.len(), 1);
        assert!(fixture.core.auth_ready_sessions.len() <= 1);
    }

    let unsigned = EventBuilder::new(Kind::Authentication, "ordinary publish forbidden")
        .build(fixture.keys.public_key());
    let effects = fixture.core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(unsigned),
            durability: Durability::Ephemeral,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: None,
        },
        Box::new(DiscardReceipt),
    ));
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::EmitReceipt(
            _,
            WriteStatus::Failed(reason)
        ) if reason == "kind:22242 is reserved for reducer-owned relay authentication"
    )));
    assert!(!effects
        .iter()
        .any(|effect| matches!(effect, Effect::PublishEvent(..))));
}

#[test]
fn only_exact_ready_wakes_the_current_waiting_auth_write_once() {
    let mut fixture = Fixture::new();
    // Reducer-KNOWN auth truth is what parks a write (#8 U2 reconciliation:
    // an unchallenged session's writes proceed on ordinary connectivity;
    // only a live non-Ready challenge negotiation for the exact session
    // parks them) — so open the challenge BEFORE publishing.
    let (_, policy) = fixture.challenge("wake");
    let event = EventBuilder::new(Kind::TextNote, "waiting auth")
        .custom_created_at(Timestamp::from(9))
        .sign_with_keys(&fixture.keys)
        .unwrap();
    let parked = fixture.core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Signed(event.clone()),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: None,
        },
        Box::new(DiscardReceipt),
    ));
    assert!(parked.iter().any(|effect| matches!(
        effect,
        Effect::EmitReceipt(_, WriteStatus::AwaitingAuth { relay })
            if relay == &fixture.session.relay
    )));
    assert!(!parked
        .iter()
        .any(|effect| matches!(effect, Effect::PublishEvent(..))));

    let (sign_token, unsigned) = fixture.allow(policy.unwrap());
    let (send_token, auth_event) = fixture.sign(sign_token, unsigned);
    fixture.send_accepted(send_token);
    let ready = fixture.ok(auth_event.id, true);
    assert_eq!(
        ready
            .iter()
            .filter(|effect| matches!(
                effect,
                Effect::PublishEvent(session, current, _)
                    if session == &fixture.session && current == &event
            ))
            .count(),
        1
    );
    assert!(!fixture
        .ok(auth_event.id, true)
        .iter()
        .any(|effect| matches!(effect, Effect::PublishEvent(..))));
}

#[test]
fn unchallenged_protected_write_parks_only_for_the_bounded_probe_then_proceeds() {
    // The reconciled schedule_ready gate, refined by #8 U4's bounded AUTH
    // discovery: a relay that never challenges must not wedge the write
    // plane. While the exact fresh generation's initial observation window
    // is still open the write parks WITHOUT consuming an attempt; the
    // transport's ordered first-read completion (`AuthProbeReleased`) then
    // releases it straight to its attempt — no auth_sessions entry, no AUTH
    // handshake.
    let mut fixture = Fixture::new();
    let event = EventBuilder::new(Kind::TextNote, "no challenge needed")
        .custom_created_at(Timestamp::from(9))
        .sign_with_keys(&fixture.keys)
        .unwrap();
    let parked = fixture.core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Signed(event.clone()),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: None,
        },
        Box::new(DiscardReceipt),
    ));
    assert!(!parked
        .iter()
        .any(|effect| matches!(effect, Effect::PublishEvent(..))));
    assert!(parked.iter().any(|effect| matches!(
        effect,
        Effect::EmitReceipt(_, WriteStatus::AwaitingAuth { relay })
            if relay == &fixture.session.relay
    )));

    let released = fixture.core.handle(EngineMsg::AuthProbeReleased(
        fixture.handle,
        fixture.session.clone(),
    ));
    assert!(released.iter().any(
        |effect| matches!(effect, Effect::ReleaseInitialRead(handle) if *handle == fixture.handle)
    ));
    assert_eq!(
        released
            .iter()
            .filter(|effect| matches!(
                effect,
                Effect::PublishEvent(session, current, _)
                    if session == &fixture.session && current == &event
            ))
            .count(),
        1
    );
    assert!(!released.iter().any(|effect| matches!(
        effect,
        Effect::EmitReceipt(_, WriteStatus::AwaitingAuth { .. })
    )));
    assert!(!fixture.core.auth_sessions.contains_key(&fixture.session));
}

#[test]
fn capability_binding_rejects_wrong_completion_and_exact_invalidation() {
    let mut fixture = Fixture::new();
    let (_, policy) = fixture.challenge("policy binding");
    let policy = policy.unwrap();
    bind(&mut fixture, &policy, AuthCapability::Policy, POLICY);
    assert!(fixture
        .core
        .handle(EngineMsg::AuthPolicyCompleted(
            policy.clone(),
            Some(AuthCapabilityInstance(999)),
            AuthPolicyOutcome::Allow,
        ))
        .is_empty());
    assert!(matches!(
        auth_phase(&fixture),
        AuthSessionPhase::AwaitingPolicy { .. }
    ));
    assert!(fixture
        .core
        .handle(EngineMsg::AuthCapabilityInvalidated(
            fixture.keys.public_key(),
            AuthCapability::Policy,
            AuthCapabilityInstance(999),
        ))
        .is_empty());
    assert!(fixture
        .core
        .handle(EngineMsg::AuthCapabilityInvalidated(
            fixture.keys.public_key(),
            AuthCapability::Policy,
            POLICY,
        ))
        .iter()
        .any(|effect| matches!(effect, Effect::RelayAuth(AuthEffect::Cancel(_)))));
    assert!(matches!(auth_phase(&fixture), AuthSessionPhase::Error));
    assert!(fixture
        .core
        .handle(EngineMsg::AuthPolicyCompleted(
            policy,
            Some(POLICY),
            AuthPolicyOutcome::Allow,
        ))
        .is_empty());

    let (_, policy) = fixture.challenge("signer binding");
    let (sign_token, unsigned) = fixture.allow(policy.unwrap());
    bind(&mut fixture, &sign_token, AuthCapability::Signer, SIGNER);
    let signed = unsigned.sign_with_keys(&fixture.keys).unwrap();
    assert!(fixture
        .core
        .handle(EngineMsg::AuthSignerCompleted(
            sign_token.clone(),
            Some(AuthCapabilityInstance(999)),
            AuthSignerOutcome::Signed(signed.clone()),
        ))
        .is_empty());
    assert!(matches!(
        auth_phase(&fixture),
        AuthSessionPhase::AwaitingSignature { .. }
    ));
    fixture.core.handle(EngineMsg::AuthCapabilityInvalidated(
        fixture.keys.public_key(),
        AuthCapability::Signer,
        SIGNER,
    ));
    assert!(matches!(auth_phase(&fixture), AuthSessionPhase::Error));
    assert!(fixture
        .core
        .handle(EngineMsg::AuthSignerCompleted(
            sign_token,
            Some(SIGNER),
            AuthSignerOutcome::Signed(signed),
        ))
        .is_empty());

    let (_, policy) = fixture.challenge("replacement then stale removal");
    let (sign_token, _) = fixture.allow(policy.unwrap());
    let replacement = AuthCapabilityInstance(77);
    bind(
        &mut fixture,
        &sign_token,
        AuthCapability::Signer,
        replacement,
    );
    assert!(fixture
        .core
        .handle(EngineMsg::AuthCapabilityInvalidated(
            fixture.keys.public_key(),
            AuthCapability::Signer,
            SIGNER,
        ))
        .is_empty());
    assert!(matches!(
        auth_phase(&fixture),
        AuthSessionPhase::AwaitingSignature { .. }
    ));
}

#[test]
fn bound_capability_removal_cancels_awaiting_send_and_relay_ack() {
    let mut awaiting_send = Fixture::new();
    let (_, policy) = awaiting_send.challenge("awaiting send");
    let (sign_token, unsigned) = awaiting_send.allow(policy.unwrap());
    let (_send_token, _) = awaiting_send.sign(sign_token, unsigned);
    awaiting_send
        .core
        .handle(EngineMsg::AuthCapabilityInvalidated(
            awaiting_send.keys.public_key(),
            AuthCapability::Signer,
            SIGNER,
        ));
    assert!(matches!(
        auth_phase(&awaiting_send),
        AuthSessionPhase::Error
    ));

    let mut awaiting_ack = Fixture::new();
    let (_, policy) = awaiting_ack.challenge("awaiting ack");
    let (sign_token, unsigned) = awaiting_ack.allow(policy.unwrap());
    let (send_token, _) = awaiting_ack.sign(sign_token, unsigned);
    awaiting_ack.send_accepted(send_token);
    awaiting_ack
        .core
        .handle(EngineMsg::AuthCapabilityInvalidated(
            awaiting_ack.keys.public_key(),
            AuthCapability::Policy,
            POLICY,
        ));
    assert!(matches!(auth_phase(&awaiting_ack), AuthSessionPhase::Error));
}

#[test]
fn disconnect_releases_every_pending_and_ready_phase_and_stale_callbacks_are_inert() {
    for phase in 0..5 {
        let mut fixture = Fixture::new();
        let (_, policy) = fixture.challenge("disconnect matrix");
        let policy = policy.unwrap();
        let mut stale = policy.clone();
        if phase >= 1 {
            let (sign_token, unsigned) = fixture.allow(policy);
            stale = sign_token.clone();
            if phase >= 2 {
                let (send_token, event) = fixture.sign(sign_token, unsigned);
                stale = send_token.clone();
                if phase >= 3 {
                    fixture.send_accepted(send_token);
                    if phase >= 4 {
                        fixture.ok(event.id, true);
                    }
                }
            }
        }
        let disconnected = fixture.core.handle(EngineMsg::RelayDisconnected(
            fixture.handle,
            fixture.session.clone(),
            DisconnectReason::Closed,
        ));
        assert!(disconnected
            .iter()
            .any(|effect| matches!(effect, Effect::RelayAuth(AuthEffect::Cancel(_)))));
        assert!(fixture.core.auth_sessions.is_empty());
        assert!(fixture.core.auth_ready_sessions.is_empty());
        assert!(fixture
            .core
            .handle(EngineMsg::AuthSendCompleted(
                stale,
                AuthSendOutcome::Accepted,
            ))
            .is_empty());
    }
}

#[test]
fn wrong_operation_and_transport_generation_tokens_are_inert() {
    let mut fixture = Fixture::new();
    let (_, policy) = fixture.challenge("wrong tokens");
    let policy = policy.unwrap();
    bind(&mut fixture, &policy, AuthCapability::Policy, POLICY);
    let mut wrong_operation = policy.clone();
    wrong_operation.sequence += 1;
    assert!(fixture
        .core
        .handle(EngineMsg::AuthPolicyCompleted(
            wrong_operation,
            Some(POLICY),
            AuthPolicyOutcome::Allow,
        ))
        .is_empty());
    let mut wrong_generation = policy.clone();
    wrong_generation.epoch.handle.generation += 1;
    assert!(fixture
        .core
        .handle(EngineMsg::AuthPolicyCompleted(
            wrong_generation,
            Some(POLICY),
            AuthPolicyOutcome::Allow,
        ))
        .is_empty());
    assert!(matches!(
        auth_phase(&fixture),
        AuthSessionPhase::AwaitingPolicy { .. }
    ));
}

#[test]
fn auth_required_closed_revokes_ready_and_restricted_closed_is_denied() {
    let mut fixture = Fixture::new();
    let (_, policy) = fixture.challenge("ready then closed");
    let (sign_token, unsigned) = fixture.allow(policy.unwrap());
    let (send_token, event) = fixture.sign(sign_token, unsigned);
    fixture.send_accepted(send_token);
    fixture.ok(event.id, true);
    let closed = fixture.core.handle(EngineMsg::RelayFrame(
        fixture.handle,
        fixture.session.clone(),
        RelayFrame::from(RelayMessage::Closed {
            subscription_id: Cow::Owned(SubscriptionId::new(wire_sub_id_string(&fixture.sub_id))),
            message: Cow::Borrowed("auth-required: authenticate again"),
        }),
    ));
    assert!(closed
        .iter()
        .any(|effect| matches!(effect, Effect::RelayAuth(AuthEffect::Cancel(_)))));
    assert!(closed
        .iter()
        .any(|effect| matches!(effect, Effect::Wire(_))));
    assert!(!fixture
        .core
        .auth_ready_sessions
        .contains_key(&fixture.session));
    assert!(matches!(auth_phase(&fixture), AuthSessionPhase::Denied));
    assert_eq!(
        EngineCore::<MemoryStore>::auth_source_status(
            &fixture.core.auth_sessions[&fixture.session]
        ),
        SourceStatus::AuthDenied
    );

    fixture.challenge("pending restricted");
    fixture.core.handle(EngineMsg::RelayFrame(
        fixture.handle,
        fixture.session.clone(),
        RelayFrame::from(RelayMessage::Closed {
            subscription_id: Cow::Owned(SubscriptionId::new(wire_sub_id_string(&fixture.sub_id))),
            message: Cow::Borrowed("restricted: not permitted"),
        }),
    ));
    assert!(matches!(auth_phase(&fixture), AuthSessionPhase::Denied));
}

#[test]
fn auth_timestamps_enforce_future_window_and_survive_backward_clock() {
    let mut fixture = Fixture::new();
    for second in 0..=AUTH_MAX_FUTURE_SECS {
        let (_, policy) = fixture.challenge("same-second");
        let (_, unsigned) = fixture.allow(policy.unwrap());
        assert_eq!(unsigned.created_at, Timestamp::from(second));
    }
    let (_, policy) = fixture.challenge("same-second");
    let policy = policy.unwrap();
    bind(&mut fixture, &policy, AuthCapability::Policy, POLICY);
    assert!(fixture
        .core
        .handle(EngineMsg::AuthPolicyCompleted(
            policy,
            Some(POLICY),
            AuthPolicyOutcome::Allow,
        ))
        .is_empty());
    assert!(matches!(auth_phase(&fixture), AuthSessionPhase::Error));

    let mut fixture = Fixture::new();
    fixture.core.tick(Timestamp::from(100));
    let (_, policy) = fixture.challenge("forward");
    let (_, first) = fixture.allow(policy.unwrap());
    fixture.core.tick(Timestamp::from(50));
    let (_, policy) = fixture.challenge("backward");
    let (_, second) = fixture.allow(policy.unwrap());
    assert_eq!(first.created_at, Timestamp::from(100));
    assert_eq!(second.created_at, Timestamp::from(101));
}

#[test]
fn auth_counter_exhaustion_is_terminal_error_without_wrap_or_request() {
    let mut epoch = Fixture::new();
    epoch.core.next_auth_epoch = None;
    let (effects, token) = epoch.challenge("epoch exhausted");
    assert!(token.is_none());
    assert!(!effects
        .iter()
        .any(|effect| matches!(effect, Effect::RelayAuth(AuthEffect::RequestPolicy { .. }))));
    assert!(matches!(auth_phase(&epoch), AuthSessionPhase::Error));

    let mut operation = Fixture::new();
    operation.core.next_auth_operation = None;
    let (_, token) = operation.challenge("operation exhausted");
    assert!(token.is_none());
    assert!(matches!(auth_phase(&operation), AuthSessionPhase::Error));

    let mut signer = Fixture::new();
    let (_, policy) = signer.challenge("sign operation exhausted");
    let policy = policy.unwrap();
    bind(&mut signer, &policy, AuthCapability::Policy, POLICY);
    signer.core.next_auth_operation = None;
    assert!(signer
        .core
        .handle(EngineMsg::AuthPolicyCompleted(
            policy,
            Some(POLICY),
            AuthPolicyOutcome::Allow,
        ))
        .is_empty());
    assert!(matches!(auth_phase(&signer), AuthSessionPhase::Error));

    let mut send = Fixture::new();
    let (_, policy) = send.challenge("send operation exhausted");
    let (sign_token, unsigned) = send.allow(policy.unwrap());
    bind(&mut send, &sign_token, AuthCapability::Signer, SIGNER);
    let signed = unsigned.sign_with_keys(&send.keys).unwrap();
    send.core.next_auth_operation = None;
    assert!(send
        .core
        .handle(EngineMsg::AuthSignerCompleted(
            sign_token,
            Some(SIGNER),
            AuthSignerOutcome::Signed(signed),
        ))
        .is_empty());
    assert!(matches!(auth_phase(&send), AuthSessionPhase::Error));
}

// NOTE: the public AUTH-diagnostics projection falsifier
// (`diagnostics_expose_bounded_safe_auth_facts`, covering the BLAKE3
// challenge descriptor and the per-phase `AuthDiagnosticsSnapshot` read-out)
// is deferred to Wave 3 with that surface. The reducer's internal phase
// truth this wave stays covered by the `auth_phase(..)`-based falsifiers
// above, which read `AuthSessionState::phase` directly.

#[test]
fn every_frozen_auth_field_id_signature_and_tag_order_are_validated() {
    for mutation in 0..9 {
        let mut fixture = Fixture::new();
        let challenge = "frozen-template";
        let (_, policy) = fixture.challenge(challenge);
        let (sign_token, unsigned) = fixture.allow(policy.unwrap());
        bind(&mut fixture, &sign_token, AuthCapability::Signer, SIGNER);
        let valid = unsigned.clone().sign_with_keys(&fixture.keys).unwrap();
        let bad = match mutation {
            0 => {
                let other = Keys::generate();
                EventBuilder::auth(challenge, fixture.session.relay.clone())
                    .custom_created_at(unsigned.created_at)
                    .sign_with_keys(&other)
                    .unwrap()
            }
            1 => {
                let mut changed = unsigned.clone();
                changed.id = None;
                changed.created_at = Timestamp::from(unsigned.created_at.as_secs() + 1);
                changed.sign_with_keys(&fixture.keys).unwrap()
            }
            2 => {
                let mut changed = unsigned.clone();
                changed.id = None;
                changed.kind = Kind::TextNote;
                changed.sign_with_keys(&fixture.keys).unwrap()
            }
            3 => {
                let mut changed = unsigned.clone();
                changed.id = None;
                changed.content = "not empty".to_string();
                changed.sign_with_keys(&fixture.keys).unwrap()
            }
            4 => EventBuilder::auth("different-challenge", fixture.session.relay.clone())
                .custom_created_at(unsigned.created_at)
                .sign_with_keys(&fixture.keys)
                .unwrap(),
            5 => EventBuilder::auth(
                challenge,
                RelayUrl::parse("wss://different-auth-relay.example").unwrap(),
            )
            .custom_created_at(unsigned.created_at)
            .sign_with_keys(&fixture.keys)
            .unwrap(),
            6 => {
                let mut tags: Vec<_> = unsigned.tags.iter().cloned().collect();
                tags.reverse();
                nostr::UnsignedEvent::new(
                    unsigned.pubkey,
                    unsigned.created_at,
                    unsigned.kind,
                    tags,
                    unsigned.content.clone(),
                )
                .sign_with_keys(&fixture.keys)
                .unwrap()
            }
            7 => {
                let mut changed = valid.clone();
                changed.id = EventId::from_hex(&"00".repeat(32)).unwrap();
                changed
            }
            8 => {
                let mut changed = valid;
                changed.sig = EventBuilder::new(Kind::TextNote, "other signature")
                    .sign_with_keys(&fixture.keys)
                    .unwrap()
                    .sig;
                changed
            }
            _ => unreachable!(),
        };
        let effects = fixture.core.handle(EngineMsg::AuthSignerCompleted(
            sign_token,
            Some(SIGNER),
            AuthSignerOutcome::Signed(bad),
        ));
        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, Effect::RelayAuth(AuthEffect::Send { .. }))));
        assert!(matches!(auth_phase(&fixture), AuthSessionPhase::Error));
    }
}

#[test]
fn slot_replacement_releases_the_displaced_session_without_waiting_for_disconnect() {
    let mut fixture = Fixture::new();
    fixture.challenge("old session");
    let old_epoch = fixture.core.auth_sessions[&fixture.session].epoch.clone();
    let replacement = RelaySessionKey::new(
        fixture.session.relay.clone(),
        AccessContext::Nip42(Keys::generate().public_key()),
    );
    let replacement_handle = RelayHandle {
        slot: fixture.handle.slot,
        generation: fixture.handle.generation + 1,
    };

    let effects = fixture.core.handle(EngineMsg::RelayConnected(
        replacement_handle,
        replacement.clone(),
    ));
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::RelayAuth(AuthEffect::Cancel(epoch)) if epoch == &old_epoch
    )));
    assert!(!fixture.core.auth_sessions.contains_key(&fixture.session));
    assert!(!fixture.core.connected_relays.contains(&fixture.session));
    assert!(fixture.core.connected_relays.contains(&replacement));
    assert!(fixture.core.auth_sessions.len() <= fixture.core.connected_relays.len());
}

/// #8 U4 latent-item hardening: `u64::MAX` is reserved BY VALUE for the
/// counter-exhausted fallback epoch, never minted as a real sequence. The
/// exhausted counter fails closed (typed `Error` state, no policy request),
/// and a token forged around the sentinel epoch cannot advance the session.
#[test]
fn auth_sequence_counter_reserves_the_sentinel_and_exhaustion_fails_closed() {
    // The last REAL mintable epoch sequence is u64::MAX - 1.
    let mut fixture = Fixture::new();
    fixture.core.next_auth_epoch = Some(u64::MAX - 1);
    let (_, policy) = fixture.challenge("last-real-epoch");
    let token = policy.expect("u64::MAX - 1 is still a real mintable epoch");
    assert_eq!(token.epoch.sequence, u64::MAX - 1);

    // A counter whose next value would be the sentinel is exhausted: the
    // challenge records the sentinel fallback epoch in phase Error and
    // requests nothing.
    let mut fixture = Fixture::new();
    fixture.core.next_auth_epoch = Some(u64::MAX);
    let (effects, policy) = fixture.challenge("sentinel-reserved");
    assert!(
        policy.is_none(),
        "an exhausted epoch counter must fail closed, never mint the sentinel as a real epoch"
    );
    assert!(!effects
        .iter()
        .any(|effect| matches!(effect, Effect::RelayAuth(AuthEffect::RequestPolicy { .. }))));
    let state = fixture.core.auth_sessions.get(&fixture.session).unwrap();
    assert_eq!(state.epoch.sequence, u64::MAX);
    assert!(matches!(state.phase, AuthSessionPhase::Error));

    // A forged operation token equal to the stored sentinel epoch can never
    // advance the session out of its failed-closed state.
    let forged = AuthOpToken {
        epoch: state.epoch.clone(),
        sequence: 1,
    };
    assert!(fixture
        .core
        .handle(EngineMsg::AuthPolicyCompleted(
            forged.clone(),
            Some(POLICY),
            AuthPolicyOutcome::Allow,
        ))
        .iter()
        .all(|effect| !matches!(
            effect,
            Effect::RelayAuth(AuthEffect::RequestSignature { .. })
        )));
    let state = fixture.core.auth_sessions.get(&fixture.session).unwrap();
    assert!(matches!(state.phase, AuthSessionPhase::Error));

    // Once exhausted, later challenges stay failed-closed instead of reusing
    // sequences.
    let (_, reissued) = fixture.challenge("still-exhausted");
    assert!(reissued.is_none());
}
