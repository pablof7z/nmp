//! #1406 falsifier.
//!
//! Traces (posted to #1406) found the settlement/deletion path for a
//! semantic (`ReplaceableOperation`) write is gated TWICE, not once:
//!
//! - the reducer, `try_close_semantic_cohort` (`core/semantic_sources.rs`):
//!   `if !matches!(&snapshot.source_policy, SemanticSourcePolicy::Finite(round) if round.is_closed()) { return; }`
//! - the store, `close_cohort` (`nmp-store/redb_store/semantic_edit_ops.rs`):
//!   `let SemanticSourcePolicy::Finite(round) = &state.source_policy else { return Ok(SourceRoundOpen); }`
//!
//! `ReplaceableSourcePolicy::Continuing` -- the value `nmp-nip02/src/edit.rs`
//! and `nmp/src/nip29/group_list_writes.rs` both unconditionally construct --
//! can never satisfy either gate. `close_if_all_lanes_terminal`
//! (`core/write.rs`) calls the reducer gate and returns immediately for every
//! `ReplaceableOperation` write, so neither `WriteFact::Outcome(WriteOutcome::Settled)`
//! nor any durable semantic-state deletion is reachable for a real semantic
//! write today. This test drives the real end-to-end reducer+store path (it
//! never constructs `Finite` itself, so a patch that fixes only one of the
//! two gates still leaves this red), and asserts on every member receipt of
//! a shared generation, not just the owner intent
//! (`generation.members.first()` is merely who the reducer's close CHECK
//! reads lane state from; the store's success arm settles every member).
//!
//! This uses a minimal in-crate materializer (`Kind::ContactList`,
//! `ReplaceableSourcePolicy::Continuing`) rather than the real `nmp-nip02`/
//! `nmp-nip29` crates: both depend on `nmp`, so an `nmp -> nmp-nip02` edge is
//! the exact cyclic dependency `crates/nmp/src/lib.rs`'s own doc comment says
//! must not exist. The defect lives in the generic mechanism every semantic
//! capability shares, not in either protocol module's own tag/kind logic, so
//! a same-policy stand-in proves the identical fault line without that edge.
//!
//! nmp:falsifier=Route both gates' fix in and this must go green: every
//! member receipt of every generation settles regardless of whether its
//! relay published or gave up, and no durable/in-memory semantic state
//! survives the last terminal lane.

use super::*;
use crate::{
    RegisteredReplaceableMaterializer, ReplaceableMaterializer, ReplaceableMaterializerOperation,
    ReplaceableMaterializerRefusal, ReplaceableSourcePolicy,
};
use nostr::{Keys, Kind, RelayMessage, RelayUrl};
use std::sync::Arc;

/// Mirrors NIP-02's `FollowMaterializer` shape closely enough to exercise the
/// same generic-mechanism path (`Kind::ContactList`, opaque operation bytes,
/// `ReplaceableSourcePolicy::Continuing`) without depending on `nmp-nip02`.
struct TinyContactListMaterializer;

impl ReplaceableMaterializer for TinyContactListMaterializer {
    fn materialize(
        &self,
        _source: &nostr::UnsignedEvent,
        current: &nostr::UnsignedEvent,
        _operations: &[ReplaceableMaterializerOperation<'_>],
    ) -> Result<nmp_grammar::EventBuilder, ReplaceableMaterializerRefusal> {
        Ok(nmp_grammar::EventBuilder {
            kind: current.kind,
            tags: current.tags.clone().to_vec().into_iter().collect(),
            content: current.content.clone(),
            created_at: None,
        })
    }

    fn materialize_default(
        &self,
        coordinate: &nostr::nips::nip01::Coordinate,
        _operations: &[ReplaceableMaterializerOperation<'_>],
    ) -> Result<nmp_grammar::EventBuilder, ReplaceableMaterializerRefusal> {
        if coordinate.kind != Kind::ContactList {
            return Err(ReplaceableMaterializerRefusal {
                reason: "test materializer only handles ContactList".to_string(),
            });
        }
        Ok(nmp_grammar::EventBuilder {
            kind: Kind::ContactList,
            tags: Vec::new().into_iter().collect(),
            content: String::new(),
            created_at: None,
        })
    }
}

fn session_for(relay: &RelayUrl, author: &Keys) -> RelaySessionKey {
    RelaySessionKey::new(relay.clone(), AccessContext::Nip42(author.public_key()))
}

/// One capability install per `EngineCore`; `instance` only needs to be
/// unique per install, never per operation.
fn install_capability(
    core: &mut EngineCore,
    instance: [u8; 16],
) -> RegisteredReplaceableMaterializer {
    core.add_replaceable_materializer(
        crate::replaceable_materializer::ReplaceableMaterializerRegistration {
            instance,
            program: *b"nmp1406-falsify1",
            format: *b"nmp1406-falsify2",
            materializer: Arc::new(TinyContactListMaterializer),
        },
    );
    RegisteredReplaceableMaterializer { instance }
}

/// One accepted+signed member intent's receipt, plus enough to find it again
/// after its shared generation's route is driven to terminal.
struct MemberReceipt {
    receipt: ReceiptId,
}

/// One generation: N member intents on the SAME author/coordinate, accepted
/// back to back before any relay connects so they merge into one
/// materialized event riding one shared route, plus that route's one
/// dedicated relay.
struct ArmedGeneration {
    relay: RelayUrl,
    members: Vec<MemberReceipt>,
    event_id: EventId,
    handle: TransportRelayHandle,
    session: RelaySessionKey,
}

/// Accept `member_count` Follow-like operations on one fresh author's
/// contact list (all still unrouted, so they merge into one generation),
/// sign each, connect the generation's one dedicated relay, and admit its
/// one attempt.
fn arm_generation(
    core: &mut EngineCore,
    registration: &RegisteredReplaceableMaterializer,
    slot: u32,
    member_count: usize,
) -> ArmedGeneration {
    assert!(member_count >= 1);
    let author = Keys::generate();
    let relay = RelayUrl::parse(&format!("wss://falsifier-{slot}.example.com")).unwrap();
    core.handle(EngineMsg::SetActivePubkey(Some(author.public_key())));

    let mut members = Vec::with_capacity(member_count);
    for member in 0..member_count {
        let payload = registration
            .first_value_operation(
                Kind::ContactList,
                String::new(),
                ReplaceableSourcePolicy::Continuing,
                vec![slot as u8, member as u8, 0],
            )
            .expect("first-value operation builds");
        let accepted = core.handle(EngineMsg::Publish(WriteIntent {
            payload,
            routing: WriteRouting::Explicit(vec![relay.clone()]),
            identity: Identity::Explicit(author.public_key()),
            correlation: None,
        }));
        let receipt = accepted
            .iter()
            .find_map(|effect| match effect {
                Effect::WriteAccepted(id, _) => Some(*id),
                _ => None,
            })
            .expect("write is accepted");
        let (sign_id, generation, unsigned) = accepted
            .iter()
            .find_map(|effect| match effect {
                Effect::RequestSign(id, generation, unsigned) => {
                    Some((*id, *generation, unsigned.clone()))
                }
                _ => None,
            })
            .expect("accepted write requests signing");
        let signed = unsigned.sign_with_keys(&author).expect("sign fixture");
        core.handle(EngineMsg::SignerCompleted(sign_id, generation, Ok(signed)));
        members.push(MemberReceipt { receipt });
    }

    let session = session_for(&relay, &author);
    let handle = TransportRelayHandle {
        slot,
        generation: 1,
    };
    core.handle(EngineMsg::RelayConnected(handle, session.clone()));
    let scheduled = core.handle(EngineMsg::AuthProbeReleased(handle, session.clone()));
    let (correlation, event_id) = scheduled
        .iter()
        .find_map(|effect| match effect {
            Effect::PublishEvent(candidate, event, correlation) if candidate == &session => {
                Some((*correlation, event.id))
            }
            _ => None,
        })
        .expect("eligible lane starts its one attempt");
    core.handle(EngineMsg::EventHandoff(correlation, HandoffResult::Written));

    ArmedGeneration {
        relay,
        members,
        event_id,
        handle,
        session,
    }
}

fn settled_receipts(effects: &[Effect]) -> BTreeSet<ReceiptId> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::EmitReceipt(id, WriteFact::Outcome(WriteOutcome::Settled)) => Some(*id),
            _ => None,
        })
        .collect()
}

/// Isolation case: TWO member intents share one `Published` generation. Both
/// must settle. Neither does.
#[test]
fn published_generation_settles_no_member() {
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 4);
    let registration = install_capability(&mut core, [1u8; 16]);
    let gen = arm_generation(&mut core, &registration, 0, 2);

    let effects = core.handle(EngineMsg::RelayFrame(
        gen.handle,
        gen.session.clone(),
        RelayFrame::from(RelayMessage::ok(gen.event_id, true, "")),
    ));
    assert!(
        effects.iter().any(|effect| matches!(
            effect,
            Effect::EmitReceipt(
                _,
                WriteFact::Relay {
                    state: RelayState::Published,
                    ..
                }
            )
        )),
        "fixture sanity: the shared relay must actually report Published, got {effects:?}"
    );
    let settled = settled_receipts(&effects);
    let expected: BTreeSet<_> = gen.members.iter().map(|m| m.receipt).collect();
    println!("PUBLISHED: settled {settled:?} of expected {expected:?}");
    assert_eq!(
        settled, expected,
        "#1406: every member of a fully-published shared generation must settle and none does"
    );
}

/// Isolation case: TWO member intents share one `GaveUp` generation (bounded
/// `max_publish_attempts`, not a timeout heuristic). Both must settle.
/// Neither does.
#[test]
fn given_up_generation_settles_no_member() {
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 4)
        .with_max_publish_attempts(1);
    let registration = install_capability(&mut core, [2u8; 16]);
    let gen = arm_generation(&mut core, &registration, 0, 2);

    let effects = core.handle(EngineMsg::RelayDisconnected(
        gen.handle,
        gen.session.clone(),
        DisconnectReason::Error,
    ));
    assert!(
        effects.iter().any(|effect| matches!(
            effect,
            Effect::EmitReceipt(_, WriteFact::Relay { state: RelayState::GaveUp, .. })
        )),
        "fixture sanity: max_publish_attempts(1) must actually give up on this drop, got {effects:?}"
    );
    let settled = settled_receipts(&effects);
    let expected: BTreeSet<_> = gen.members.iter().map(|m| m.receipt).collect();
    println!("GAVE UP: settled {settled:?} of expected {expected:?}");
    assert_eq!(
        settled, expected,
        "#1406: every member of a shared generation whose relay gave up must settle and none does"
    );
}

/// #1406's settlement falsifier: N generations, each 2 member intents
/// sharing one materialized event over one dedicated relay, driven to a MIX
/// of terminal outcomes -- half `Published`, half `GaveUp`
/// (`max_publish_attempts(1)`, the real bounded retry-deadline machinery,
/// not a happy-path-only or owner-only proof). Every member receipt of every
/// generation must reach `WriteOutcome::Settled` once its lane is terminal.
/// None does, for any of them, regardless of N.
#[test]
fn n_generations_mixed_outcomes_no_member_ever_settles() {
    const GENERATIONS: usize = 4;
    const MEMBERS_PER_GENERATION: usize = 2;
    assert_eq!(
        GENERATIONS % 2,
        0,
        "fixture wants an even split of published/given-up"
    );

    let mut core = EngineCore::new(
        RedbStore::temporary().expect("temporary Redb store"),
        GENERATIONS + 2,
    )
    .with_max_publish_attempts(1);
    let registration = install_capability(&mut core, [3u8; 16]);

    let generations: Vec<ArmedGeneration> = (0..GENERATIONS as u32)
        .map(|slot| arm_generation(&mut core, &registration, slot, MEMBERS_PER_GENERATION))
        .collect();

    let mut settled_total = 0usize;
    let mut expected_total = 0usize;
    let mut terminal_states = Vec::with_capacity(GENERATIONS);
    for (index, gen) in generations.into_iter().enumerate() {
        let expected: BTreeSet<_> = gen.members.iter().map(|m| m.receipt).collect();
        expected_total += expected.len();

        let (label, terminal_effects) = if index % 2 == 0 {
            let effects = core.handle(EngineMsg::RelayFrame(
                gen.handle,
                gen.session.clone(),
                RelayFrame::from(RelayMessage::ok(gen.event_id, true, "")),
            ));
            ("Published", effects)
        } else {
            let effects = core.handle(EngineMsg::RelayDisconnected(
                gen.handle,
                gen.session.clone(),
                DisconnectReason::Error,
            ));
            ("GaveUp", effects)
        };
        assert!(
            !terminal_effects.is_empty(),
            "generation {index} ({label}) over {} produced no effect at all",
            gen.relay,
        );
        terminal_states.push((label, gen.relay));
        settled_total += settled_receipts(&terminal_effects)
            .intersection(&expected)
            .count();
    }

    println!("terminal states driven: {terminal_states:?}");
    println!(
        "settled: {settled_total} of {expected_total} member receipts across {GENERATIONS} \
         generations ({MEMBERS_PER_GENERATION} members each)"
    );
    assert_eq!(
        settled_total, expected_total,
        "#1406: {settled_total} of {expected_total} member receipts settled after every \
         generation's shared route went terminal (mix of Published/GaveUp, {MEMBERS_PER_GENERATION} \
         members sharing each generation) -- every one of them should have settled and none did, \
         because BOTH try_close_semantic_cohort's (core/semantic_sources.rs) and close_cohort's \
         (nmp-store/redb_store/semantic_edit_ops.rs) Finite(..).is_closed() gates can never pass \
         while ReplaceableSourcePolicy::Continuing is the only policy any capability constructs \
         (nmp-nip02/src/edit.rs, nmp/src/nip29/group_list_writes.rs)"
    );
}
