//! Plan-centric public evidence across NIP-77 child roles.

use super::*;

struct Nip77StatusFixture {
    core: EngineCore<MemoryStore>,
    session: RelaySessionKey,
    handle: TransportRelayHandle,
    observation: ObservationId,
    plan_sub_id: SubId,
    neg_sub_id: SubId,
    neg_attempt: RequestAttemptId,
    initial_hex: String,
}

impl Nip77StatusFixture {
    fn open() -> Self {
        let relay = RelayUrl::parse("wss://request-attempt-nip77-phases.example").unwrap();
        let session = RelaySessionKey::public(relay.clone());
        let handle = TransportRelayHandle {
            slot: 98,
            generation: 1,
        };
        let mut core = EngineCore::new(MemoryStore::new(), 8);
        core.prober
            .states
            .insert(relay.clone(), crate::negentropy::ProbeState::Supported);
        core.handle(EngineMsg::RelayConnected(handle, session.clone()));
        let opened = core.handle(EngineMsg::Subscribe(live_query(&relay)));
        let observation = observation_id(&opened);

        let flushed = core.handle(EngineMsg::FlushWireAdmission);
        let (_, candidate_sub_id, _, candidate_attempt) = only_request(&flushed);
        let plan_sub_id = core.router.plan().reqs[&session][0].sub_id.clone();
        assert_ne!(candidate_sub_id, plan_sub_id);
        assert_status(&flushed, SourceStatus::AwaitingRequest);
        assert_no_error(&flushed);

        let accepted = core.on_wire_request_handoff(RequestHandoffOutcome::Accepted {
            attempt_id: candidate_attempt,
            handle,
        });
        assert_status(&accepted, SourceStatus::Requesting);
        assert_no_error(&accepted);

        let candidate_eose = core.on_relay_frame(
            handle,
            session.clone(),
            RelayFrame::from_message(RelayMessage::EndOfStoredEvents(Cow::Owned(
                nostr::SubscriptionId::new(wire_sub_id_string(&candidate_sub_id)),
            ))),
        );
        let (neg_attempt, neg_sub_id, initial_hex) = candidate_eose
            .iter()
            .find_map(|effect| match effect {
                Effect::NegOpen(attempt, _, sub_id, _, initial) => {
                    Some((*attempt, sub_id.clone(), initial.clone()))
                }
                _ => None,
            })
            .expect("the accepted candidate EOSE opens reconciliation");

        Self {
            core,
            session,
            handle,
            observation,
            plan_sub_id,
            neg_sub_id,
            neg_attempt,
            initial_hex,
        }
    }

    fn accept_neg_open(&mut self) -> Vec<Effect> {
        self.core.on_nip77_handoff(
            Nip77Frame::Open,
            RequestHandoffOutcome::Accepted {
                attempt_id: self.neg_attempt,
                handle: self.handle,
            },
        )
    }

    fn respond(&mut self, response_hex: String) -> Vec<Effect> {
        self.core.on_relay_frame(
            self.handle,
            self.session.clone(),
            RelayFrame::from_message(RelayMessage::NegMsg {
                subscription_id: Cow::Owned(nostr::SubscriptionId::new(wire_sub_id_string(
                    &self.neg_sub_id,
                ))),
                message: Cow::Owned(response_hex),
            }),
        )
    }

    fn close(mut self) {
        self.core.handle(EngineMsg::Unsubscribe(self.observation));
        assert_eq!(
            self.core.bench_ownership_census(),
            CoreOwnershipCensus::default()
        );
    }
}

fn responder(
    items: &[(u64, [u8; 32])],
) -> ::negentropy::Negentropy<'static, ::negentropy::NegentropyStorageVector> {
    let mut storage = ::negentropy::NegentropyStorageVector::new();
    for (timestamp, id) in items {
        storage
            .insert(*timestamp, ::negentropy::Id::from_byte_array(*id))
            .expect("insert responder item");
    }
    storage.seal().expect("seal responder storage");
    ::negentropy::Negentropy::owned(storage, 0).expect("construct responder")
}

fn response(
    server: &mut ::negentropy::Negentropy<'_, ::negentropy::NegentropyStorageVector>,
    client_hex: &str,
) -> String {
    let bytes = hex::decode(client_hex).expect("decode client message");
    let response = server.reconcile(&bytes).expect("reconcile server response");
    hex::encode(response)
}

fn assert_status(effects: &[Effect], expected: SourceStatus) {
    assert!(
        statuses(effects).contains(&expected),
        "expected {expected:?}, got {:?}",
        statuses(effects)
    );
}

fn assert_no_error(effects: &[Effect]) {
    assert!(!statuses(effects).contains(&SourceStatus::Error));
}

fn awaiting_precedes_wire(effects: &[Effect]) {
    let awaiting = effects
        .iter()
        .position(|effect| {
            matches!(effect, Effect::EmitRows(..))
                && statuses(std::slice::from_ref(effect)).contains(&SourceStatus::AwaitingRequest)
        })
        .expect("plan evidence reports the pending local placement");
    let wire = effects
        .iter()
        .position(|effect| matches!(effect, Effect::Wire(_)))
        .expect("the role emits its request after evidence");
    assert!(awaiting < wire);
    assert_no_error(effects);
}

#[test]
fn empty_neg_completion_projects_finished_status_through_the_plan_request() {
    let mut fixture = Nip77StatusFixture::open();
    let accepted = fixture.accept_neg_open();
    assert_no_error(&accepted);

    let mut server = responder(&[]);
    let initial = fixture.initial_hex.clone();
    let completed = fixture.respond(response(&mut server, &initial));
    assert_status(&completed, SourceStatus::FinishedStoredEvents);
    assert_no_error(&completed);
    fixture.close();
}

#[test]
fn refused_neg_open_publishes_awaiting_before_its_fallback_request() {
    let mut fixture = Nip77StatusFixture::open();
    let fallback = fixture.core.on_nip77_handoff(
        Nip77Frame::Open,
        RequestHandoffOutcome::Refused {
            attempt_id: fixture.neg_attempt,
            cause: LocalSendRefusal::SessionUnavailable,
        },
    );
    awaiting_precedes_wire(&fallback);
    fixture.close();
}

#[test]
fn missing_id_backfill_publishes_awaiting_before_its_request() {
    let mut fixture = Nip77StatusFixture::open();
    fixture.accept_neg_open();
    let mut server = responder(&[(7, [7; 32])]);
    let mut client_hex = fixture.initial_hex.clone();

    loop {
        let effects = fixture.respond(response(&mut server, &client_hex));
        if effects
            .iter()
            .any(|effect| matches!(effect, Effect::Wire(_)))
        {
            awaiting_precedes_wire(&effects);
            break;
        }
        let (attempt_id, next) = effects
            .iter()
            .find_map(|effect| match effect {
                Effect::NegMsg(attempt, _, sub_id, next) if sub_id == &fixture.neg_sub_id => {
                    Some((*attempt, next.clone()))
                }
                _ => None,
            })
            .expect("reconciliation continues or opens its missing-id request");
        fixture.core.on_nip77_handoff(
            Nip77Frame::Continue,
            RequestHandoffOutcome::Accepted {
                attempt_id,
                handle: fixture.handle,
            },
        );
        client_hex = next;
    }

    assert!(fixture
        .core
        .plan_execution_metadata
        .contains_key(&fixture.plan_sub_id));
    fixture.close();
}
