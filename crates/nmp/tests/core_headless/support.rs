//! Headless `EngineCore` tests (M3 plan §5 tier A, re-expressed at the
//! `EngineCore` level per the M3-B build brief) + the coverage-attribution
//! request-attribution falsifiers
//! (`docs/design/query-demand-and-evidence.md`, issue #816). Zero I/O:
//! every "relay" interaction here is a scripted `EngineMsg::RelayConnected`/
//! `RelayFrame` fed directly to `EngineCore::handle`, exactly as the ruling's
//! own reasoning demands (send-time snapshots, the EOSE intersection rule,
//! `limit` poisoning, and per-query scoped acquisition evidence).

use std::borrow::Cow;
use std::time::{Duration, Instant};

use nmp::mechanism::core::{
    AcquisitionEvidence, AuthCapability, AuthCapabilityInstance, AuthEffect, AuthPolicyOutcome,
    AuthSendCompletion, AuthSendOutcome, AuthSignerOutcome, Effect, EngineCore, EngineMsg,
    LocalSendRefusal, Nip77Frame, ObservationFact, ObservationId, PublishError, ReceiptId,
    RequestAttemptId, RequestHandoffOutcome, RequestTerminal, RowDelta, ShortfallFact,
    SourceEvidence, SourceStatus,
};
use nmp::mechanism::publish_queue::{
    NotSentReason, RelayState, RelayWaiting, RetryCause, SigningState, WriteFact, WriteOutcome,
};
use nmp_grammar::LiveQuery;
use nmp_grammar::{
    AccessContext, Binding, ConcreteFilter, ContextualAtom, Filter, Identity, RelaySessionKey,
    SourceAuthority, WriteIntent, WritePayload, WriteRouting,
};
use nmp_router::{FixtureRoutingFacts, SubId, WireOp};
use nmp_store::{
    AcceptWrite, CoverageInterval, DurabilityOutcome, EventStore, PersistenceFault,
    PublishQueueAttemptOutcome, RedbStore, RelayObserved,
};
use nmp_transport::{DisconnectReason, HandoffResult, RelayFrame, RelayHandle};
use nostr::{Keys, Kind, RelayMessage, RelayUrl, SubscriptionId, Timestamp, UnsignedEvent};

use std::collections::BTreeSet;

/// Most headless integration scenarios model a completed admission boundary,
/// not the runtime timer itself. Keep that boundary explicit so admission
/// window tests can exercise the two reducer turns separately.
trait HeadlessAdmission {
    fn handle_and_flush(&mut self, message: EngineMsg) -> Vec<Effect>;
}

impl HeadlessAdmission for EngineCore {
    fn handle_and_flush(&mut self, message: EngineMsg) -> Vec<Effect> {
        let mut effects = self.handle(message);
        effects.extend(self.handle(EngineMsg::FlushWireAdmission(Timestamp::from(0u64))));
        effects
    }
}

fn effect_row_delta_count(effects: &[Effect]) -> usize {
    effects
        .iter()
        .map(|effect| match effect {
            Effect::EmitRows(_, deltas, _) => deltas.len(),
            _ => 0,
        })
        .sum()
}

/// A minimal note whose `created_at` is stated so the assertions below can
/// name exact ids and orderings. It takes no author: a builder has none, and
/// the write's identity decides it at acceptance.
fn draft(seq: u64, content: &str) -> nmp_grammar::EventBuilder {
    nmp_grammar::EventBuilder::new(Kind::TextNote)
        .content(content)
        .created_at(Timestamp::from(seq))
}

/// The event `draft` describes once acceptance has resolved `keys` as its
/// author -- i.e. exactly what a signer is handed and hands back.
fn signed_draft(builder: &nmp_grammar::EventBuilder, keys: &Keys) -> nostr::Event {
    nostr::UnsignedEvent::new(
        keys.public_key(),
        builder
            .created_at
            .expect("fixture drafts state their timestamp"),
        builder.kind,
        builder.tags.clone(),
        builder.content.clone(),
    )
    .sign_with_keys(keys)
    .expect("fixture signing never fails")
}

fn cf(kinds: &[u16], authors: &[&str]) -> ConcreteFilter {
    ConcreteFilter {
        kinds: Some(kinds.iter().copied().collect()),
        authors: Some(authors.iter().map(|s| s.to_string()).collect()),
        ..ConcreteFilter::default()
    }
}

/// An `AuthorOutboxes`-sourced atom (#118): every `cf(...)` fixture in this
/// file is author-bearing, so this is the exact true context each one was
/// actually acquired under -- `EngineCore::get_coverage` now takes the
/// atom's real `ContextualAtom`, never a reconstruction.
fn ctx_atom(filter: ConcreteFilter) -> ContextualAtom {
    ctx_atom_with(filter, SourceAuthority::AuthorOutboxes)
}

fn ctx_atom_with(filter: ConcreteFilter, source: SourceAuthority) -> ContextualAtom {
    ContextualAtom {
        filter,
        source,
        access: AccessContext::Public,
        routing_evidence: BTreeSet::new(),
    }
}

fn literal_query(kinds: &[u16], author_hex: &str) -> LiveQuery {
    LiveQuery::from_filter(Filter {
        kinds: Some(kinds.iter().copied().collect()),
        authors: Some(Binding::Literal(BTreeSet::from([author_hex.to_string()]))),
        ..Filter::default()
    })
}

fn new_core(dir: FixtureRoutingFacts) -> EngineCore {
    EngineCore::new_with_fixture_routing_facts(
        RedbStore::temporary().expect("temporary Redb store"),
        dir,
        10,
    )
}

/// A core whose per-relay attempt ceiling (#1031) is deliberately out of the
/// way. The ceiling is its own falsifier; a test about replay PAGING must not
/// quietly turn into a test about giving up when its retry loop crosses 16.
fn new_core_without_attempt_ceiling(dir: FixtureRoutingFacts) -> EngineCore {
    new_core(dir).with_max_publish_attempts(u64::MAX)
}

fn activate(core: &mut EngineCore, keys: &Keys) {
    core.handle(EngineMsg::SetActivePubkey(Some(keys.public_key())));
}

/// Find the single `WireOp::Req` for `relay` inside `effects`, panicking if
/// there isn't exactly one (test-fixture convenience, not production code).
fn req_for<'a>(effects: &'a [Effect], relay: &RelayUrl) -> (&'a SubId, &'a ConcreteFilter) {
    for effect in effects {
        if let Effect::Wire(delta) = effect {
            for (r, ops) in &delta.ops {
                if &r.relay == relay {
                    for op in ops {
                        if let WireOp::Req(sub_id, filter) = op {
                            return (sub_id, filter);
                        }
                    }
                }
            }
        }
    }
    panic!("expected a WireOp::Req for {relay:?} in {effects:?}");
}

fn req_for_kind<'a>(
    effects: &'a [Effect],
    relay: &RelayUrl,
    kind: u16,
) -> (&'a SubId, &'a ConcreteFilter) {
    for effect in effects {
        if let Effect::Wire(delta) = effect {
            for (r, ops) in &delta.ops {
                if &r.relay != relay {
                    continue;
                }
                for op in ops {
                    if let WireOp::Req(sub_id, filter) = op {
                        if filter
                            .kinds
                            .as_ref()
                            .is_some_and(|kinds| kinds.contains(&kind))
                        {
                            return (sub_id, filter);
                        }
                    }
                }
            }
        }
    }
    panic!("expected a kind:{kind} WireOp::Req for {relay:?} in {effects:?}");
}

fn wire_sub_string(sub_id: &SubId) -> String {
    format!("{}", sub_id.1)
}

/// Every subscription `effects` withdraws from `relay`.
fn wire_closes(effects: &[Effect], relay: &RelayUrl) -> BTreeSet<SubId> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Wire(delta) => Some(delta),
            _ => None,
        })
        .flat_map(|delta| delta.ops.iter())
        .filter(|(session, _)| &session.relay == relay)
        .flat_map(|(_, ops)| ops.iter())
        .filter_map(|op| match op {
            WireOp::Close(sub_id) => Some(sub_id.clone()),
            WireOp::Req(..) => None,
        })
        .collect()
}

fn public_session(relay: &RelayUrl) -> RelaySessionKey {
    RelaySessionKey::public(relay.clone())
}

// With the #8 AUTH reducer landed, the write plane rides the signing
// identity's authenticated session again: every durable/ephemeral write
// demands `AccessContext::Nip42(signing pubkey)`, so tests that expect
// attempts must connect exactly this session.
fn signer_session(relay: &RelayUrl, signer: nostr::PublicKey) -> RelaySessionKey {
    RelaySessionKey::new(relay.clone(), AccessContext::Nip42(signer))
}

fn protected_pinned_query(relay: &RelayUrl, signer: nostr::PublicKey, kind: u16) -> LiveQuery {
    LiveQuery::single(
        nmp_grammar::Demand::new(
            Filter {
                kinds: Some(BTreeSet::from([kind])),
                ..Filter::default()
            },
            SourceAuthority::Pinned(BTreeSet::from([relay.clone()])),
            AccessContext::Nip42(signer),
        )
        .expect("protected pinned demand is valid"),
    )
}

fn subscribed_handle(effects: &[Effect]) -> ObservationId {
    effects
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitRows(id, ..) => Some(*id),
            _ => None,
        })
        .expect("subscribe emits its initial row snapshot")
}

fn assert_no_protected_req(effects: &[Effect], session: &RelaySessionKey) {
    assert!(
        !effects.iter().any(|effect| match effect {
            Effect::Replay(candidate, reqs) => candidate == session && !reqs.is_empty(),
            Effect::Wire(delta) => delta.ops.iter().any(|(candidate, ops)| {
                candidate == session && ops.iter().any(|op| matches!(op, WireOp::Req(..)))
            }),
            _ => false,
        }),
        "protected REQs must remain parked before current AUTH readiness: {effects:?}"
    );
}

fn connect(core: &mut EngineCore, slot: u32, url: &RelayUrl) -> Vec<Effect> {
    let mut effects = core.handle(EngineMsg::RelayConnected(
        RelayHandle {
            slot,
            generation: 1,
        },
        public_session(url),
    ));
    // Most legacy headless tests model a relay with no NIP-11 support list.
    // Resolve that one-shot explicitly now that connection and HTTP
    // capability acquisition are separate reducer inputs.
    effects.extend(core.handle(EngineMsg::RelayInformationResolved(url.clone(), None)));
    effects
}

fn connect_signer(
    core: &mut EngineCore,
    slot: u32,
    url: &RelayUrl,
    signer: nostr::PublicKey,
) -> Vec<Effect> {
    let mut effects = core.handle(EngineMsg::RelayConnected(
        RelayHandle {
            slot,
            generation: 1,
        },
        signer_session(url, signer),
    ));
    effects.extend(core.handle(EngineMsg::RelayInformationResolved(url.clone(), None)));
    effects
}

fn release_author_probe(
    core: &mut EngineCore,
    handle: RelayHandle,
    url: &RelayUrl,
    signer: nostr::PublicKey,
) -> Vec<Effect> {
    core.handle(EngineMsg::AuthProbeReleased(
        handle,
        signer_session(url, signer),
    ))
}

/// Complete the canonical NIP-42 handshake for one exact signer session.
///
/// Protected-write tests call this explicitly after `connect_signer`; the
/// returned effects are the matching AUTH `OK` wake, so callers can still
/// assert any write scheduling caused by readiness.
fn authenticate_signer(
    core: &mut EngineCore,
    slot: u32,
    url: &RelayUrl,
    signer: &Keys,
) -> Vec<Effect> {
    authenticate_signer_generation(
        core,
        RelayHandle {
            slot,
            generation: 1,
        },
        url,
        signer,
    )
}

fn authenticate_signer_generation(
    core: &mut EngineCore,
    handle: RelayHandle,
    url: &RelayUrl,
    signer: &Keys,
) -> Vec<Effect> {
    let session = signer_session(url, signer.public_key());
    let challenge = core.handle(EngineMsg::RelayFrame(
        handle,
        session.clone(),
        RelayFrame::from(RelayMessage::Auth {
            challenge: Cow::Owned(format!(
                "core-headless-{}-{}",
                handle.slot, handle.generation
            )),
        }),
    ));
    let policy_token = challenge
        .into_iter()
        .find_map(|effect| match effect {
            Effect::RelayAuth(AuthEffect::RequestPolicy { token, .. }) => Some(token),
            _ => None,
        })
        .expect("AUTH challenge requests policy for the exact session");
    assert_eq!(policy_token.epoch.session, session);
    assert_eq!(policy_token.epoch.handle, handle);

    finish_authentication(core, handle, session, signer, policy_token)
}

fn finish_authentication(
    core: &mut EngineCore,
    handle: RelayHandle,
    session: RelaySessionKey,
    signer: &Keys,
    policy_token: nmp::mechanism::core::AuthOpToken,
) -> Vec<Effect> {
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
        .expect("allowed AUTH policy requests the frozen event signature");
    assert_eq!(sign_token.epoch.session, session);
    assert_eq!(sign_token.epoch.handle, handle);
    assert_eq!(unsigned.kind, Kind::Authentication);
    assert_eq!(unsigned.pubkey, signer.public_key());

    let signed = unsigned
        .sign_with_keys(signer)
        .expect("sign deterministic AUTH fixture");
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
                assert_eq!(token.epoch.session, session);
                assert_eq!(token.epoch.handle, handle);
                Some((token, event))
            }
            _ => None,
        })
        .expect("signed AUTH requests an exact-generation send");
    core.handle(EngineMsg::AuthSendCompleted(
        AuthSendCompletion::for_operation(&send_token, AuthSendOutcome::Accepted),
    ));
    core.handle(EngineMsg::RelayFrame(
        handle,
        session,
        RelayFrame::from(RelayMessage::ok(auth_event.id, true, "authenticated")),
    ))
}

fn nip11_evidence(
    supported_nips: Option<Vec<u16>>,
) -> nmp::mechanism::relay_information_service::RelayInformationCapabilityEvidence {
    nip11_evidence_until(supported_nips, u64::MAX)
}

fn nip11_evidence_until(
    supported_nips: Option<Vec<u16>>,
    fresh_until: u64,
) -> nmp::mechanism::relay_information_service::RelayInformationCapabilityEvidence {
    nmp::mechanism::relay_information_service::RelayInformationCapabilityEvidence {
        supported_nips,
        max_subscriptions: None,
        max_subid_length: None,
        document_revision: "test-revision".to_string(),
        fresh_until,
        last_error: None,
    }
}

fn mark_written(core: &mut EngineCore, effects: &[Effect], relay: &RelayUrl) -> Vec<Effect> {
    let correlation = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::PublishEvent(candidate, event, correlation)
                if &candidate.relay == relay
                    && candidate.access == AccessContext::Nip42(event.pubkey) =>
            {
                Some(*correlation)
            }
            _ => None,
        })
        .unwrap_or_else(|| {
            panic!("expected a persisted scheduled publish for connected relay: {effects:?}")
        });
    core.handle(EngineMsg::EventHandoff(correlation, HandoffResult::Written))
}

fn publish_explicit(
    core: &mut EngineCore,
    author: &Keys,
    relays: impl IntoIterator<Item = RelayUrl>,
) -> (ReceiptId, nostr::Event, Vec<Effect>) {
    activate(core, author);
    let accepted = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(draft(85, "attempt-start failure")),
        routing: WriteRouting::Explicit(Vec::from_iter(relays)),
        identity: Identity::Active,
        correlation: None,
    }));
    let (id, generation, unsigned) = find_sign_request(&accepted);
    let signed = unsigned.sign_with_keys(author).expect("sign fixture event");
    let effects = core.handle(EngineMsg::SignerCompleted(
        id,
        generation,
        Ok(signed.clone()),
    ));
    (id, signed, effects)
}

/// The two shapes a local-persistence stall takes. They are one variant now
/// and are told apart by `detail` alone, so the exact sentences are stated
/// here: a silent change to either one must fail loudly rather than quietly
/// erase the distinction the two old spellings carried in their names.
const ATTEMPT_STALL_DETAIL: &str =
    "the durable attempt fact could not be committed; no wire EVENT was emitted and recovery \
     rediscovers this exact relay from its committed route revision";
const ROUTE_STALL_DETAIL: &str =
    "the append-only route revision could not be committed; this exact relay URL is not claimed \
     to survive a crash";

/// The attempt-log stall: the relay URL survives a crash, the attempt fact
/// does not.
fn attempt_stalled(event_id: nostr::EventId, relay: &RelayUrl) -> WriteFact {
    WriteFact::Relay {
        event_id,
        relay: relay.clone(),
        state: RelayState::Waiting(RelayWaiting::PersistenceStalled {
            detail: ATTEMPT_STALL_DETAIL.to_string(),
        }),
    }
}

/// The route-revision stall: not even the resolved relay URL is claimed to
/// survive a crash.
fn route_stalled(event_id: nostr::EventId, relay: &RelayUrl) -> WriteFact {
    WriteFact::Relay {
        event_id,
        relay: relay.clone(),
        state: RelayState::Waiting(RelayWaiting::PersistenceStalled {
            detail: ROUTE_STALL_DETAIL.to_string(),
        }),
    }
}

fn receipt_statuses(effects: &[Effect]) -> Vec<WriteFact> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::EmitReceipt(_, status) => Some(status.clone()),
            _ => None,
        })
        .collect()
}

fn event_frame(sub: &str, event: nostr::Event) -> RelayFrame {
    RelayFrame::from(RelayMessage::event(SubscriptionId::new(sub), event))
}

fn eose_frame(sub: &str) -> RelayFrame {
    RelayFrame::from(RelayMessage::eose(SubscriptionId::new(sub)))
}

fn neg_msg_frame(sub: &str, message_hex: &str) -> RelayFrame {
    RelayFrame::from(RelayMessage::NegMsg {
        subscription_id: Cow::Owned(SubscriptionId::new(sub)),
        message: Cow::Owned(message_hex.to_string()),
    })
}

fn find_sign_request(effects: &[Effect]) -> (nmp::mechanism::core::ReceiptId, u64, UnsignedEvent) {
    effects
        .iter()
        .find_map(|effect| match effect {
            Effect::RequestSign(id, generation, unsigned) => {
                Some((*id, *generation, unsigned.clone()))
            }
            _ => None,
        })
        .expect("expected a RequestSign effect")
}

fn all_row_deltas(effects: &[Effect]) -> Vec<&RowDelta> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::EmitRows(_, rows, _) => Some(rows.iter()),
            _ => None,
        })
        .flatten()
        .collect()
}

#[path = "authentication.rs"]
mod authentication;
#[path = "derived_tag_fanout.rs"]
mod derived_tag_fanout;
#[path = "live_queries.rs"]
mod live_queries;
#[path = "negentropy.rs"]
mod negentropy;
#[path = "nip29_group_reads.rs"]
mod nip29_group_reads;
#[path = "optimistic_publish_projection.rs"]
mod optimistic_publish_projection;
#[path = "persistence_failures.rs"]
mod persistence_failures;
#[path = "real_corpus_benchmark.rs"]
mod real_corpus_benchmark;
#[path = "stalled_writes.rs"]
mod stalled_writes;
#[path = "state_maintenance.rs"]
mod state_maintenance;
#[path = "subscription_budget.rs"]
mod subscription_budget;
#[path = "write_publish_queue.rs"]
mod write_publish_queue;
#[path = "write_scheduling.rs"]
mod write_scheduling;
#[path = "write_state.rs"]
mod write_state;
