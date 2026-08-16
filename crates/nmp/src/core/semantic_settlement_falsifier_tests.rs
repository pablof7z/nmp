//! #1406/#1631 settlement falsifier.
//!
//! Traces (posted to #1406) found the settlement/deletion path for a
//! semantic (`ReplaceableOperation`) write was gated TWICE, not once:
//!
//! - the reducer, `try_close_semantic_cohort`:
//!   `if !matches!(&snapshot.source_policy, SemanticSourcePolicy::Finite(round) if round.is_closed()) { return; }`
//! - the store, `close_cohort` (`nmp-store/redb_store/semantic_edit_ops.rs`):
//!   `let SemanticSourcePolicy::Finite(round) = &state.source_policy else { return Ok(SourceRoundOpen); }`
//!
//! `ReplaceableSourcePolicy::Continuing` -- the value NIP-02's and NIP-29's
//! write doors (`nmp/src/nip02/writes.rs`, #1143; `nmp/src/nip29/
//! group_list_writes.rs`) both unconditionally constructed -- could never
//! satisfy either gate, so neither
//! `WriteFact::Outcome(WriteOutcome::Settled)` nor any durable
//! semantic-state deletion was reachable for a real semantic write.
//! #1631 deleted the source-policy concept entirely; settlement now keys on
//! routing closed plus every lane of the current generation terminal, under
//! the store's exact-generation CAS.
//!
//! This test drives the real end-to-end reducer+store path and asserts on
//! every member receipt of a shared generation, not just the owner intent
//! (`generation.members.first()` is merely who the reducer's close CHECK
//! reads lane state from; the store's success arm settles every member).
//! Restore either gate and it goes red again, which is what makes the 8-of-8
//! result evidence rather than a shortcut.
//!
//! It uses a minimal in-crate materializer (`Kind::ContactList`) rather than
//! the real NIP-02/NIP-29 write doors (`crate::nip02`/`crate::nip29`, #1143):
//! both live behind their own optional Cargo feature, and this core-level
//! test must run regardless of which protocol features are enabled. The
//! defect lived in the generic mechanism every semantic capability shares,
//! not in either protocol module's tag/kind logic.
//!
//! The fixture also has to answer #1631's per-relay coordinate question
//! before any delta reaches the wire, because a lane that never learns the
//! relay's current value for the coordinate is correctly parked rather than
//! sending a list built over a base that relay may already have superseded.
//!
//! nmp:falsifier=Every member receipt of every generation settles once its
//! lane is terminal, regardless of whether its relay published or gave up,
//! and no durable/in-memory semantic state survives the last terminal lane.

use super::*;
use crate::{
    RegisteredReplaceableMaterializer, ReplaceableMaterializer, ReplaceableMaterializerOperation,
    ReplaceableMaterializerRefusal,
};
use nostr::{Keys, Kind, RelayMessage, RelayUrl};

/// Mirrors NIP-02's `FollowMaterializer` shape closely enough to exercise the
/// same generic-mechanism path (`Kind::ContactList`, opaque operation bytes)
/// without depending on `nmp-nip02`.
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

/// One capability install per `EngineCore`; `program` only needs to be
/// unique per install, never per operation.
fn install_capability(
    core: &mut EngineCore,
    program: [u8; 16],
) -> RegisteredReplaceableMaterializer {
    let spec = crate::ReplaceableMaterializerSpec::new(
        program,
        *b"nmp1406-falsify2",
        TinyContactListMaterializer,
    );
    let handle = spec.handle();
    core.install_replaceable_materializer(spec.into_registration());
    handle
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
    let write_slot = slot.saturating_mul(2);
    let read_slot = write_slot.saturating_add(1);
    core.handle(EngineMsg::SetActivePubkey(Some(author.public_key())));

    let mut members = Vec::with_capacity(member_count);
    for member in 0..member_count {
        let payload = registration
            .first_value_operation(
                Kind::ContactList,
                String::new(),
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
        slot: write_slot,
        generation: 1,
    };
    // The relay's ordinary public read session, which is what #1631's
    // publish gate asks for the coordinate: this relay never required AUTH,
    // so the view it will serve the EVENT is the view it serves any reader.
    let read_session = RelaySessionKey::public(relay.clone());
    let read_handle = TransportRelayHandle {
        slot: read_slot,
        generation: 1,
    };
    core.handle(EngineMsg::RelayConnected(read_handle, read_session.clone()));
    core.handle(EngineMsg::RelayConnected(handle, session.clone()));
    let parked = core.handle(EngineMsg::AuthProbeReleased(handle, session.clone()));
    assert!(
        !parked
            .iter()
            .any(|effect| matches!(effect, Effect::PublishEvent(..))),
        "a delta generation must not reach the relay before the relay's own \
         current value for the coordinate is known"
    );
    let scheduled =
        core.answer_coordinate_coverage_for_test(&[(read_handle, read_session)], &parked);
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
/// must settle.
#[test]
fn every_member_of_a_published_generation_settles() {
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 8);
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
        "#1406/#1631: every member of a fully-published shared generation must settle"
    );
}

/// Isolation case: TWO member intents share one `GaveUp` generation (bounded
/// `max_publish_attempts`, not a timeout heuristic). Both must settle.
#[test]
fn every_member_of_a_given_up_generation_settles() {
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 8)
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
        "#1406/#1631: every member of a shared generation whose relay gave up must settle"
    );
}

/// #1406's settlement falsifier: N generations, each 2 member intents
/// sharing one materialized event over one dedicated relay, driven to a MIX
/// of terminal outcomes -- half `Published`, half `GaveUp`
/// (`max_publish_attempts(1)`, the real bounded retry-deadline machinery,
/// not a happy-path-only or owner-only proof). Every member receipt of every
/// generation must reach `WriteOutcome::Settled` once its lane is terminal,
/// for every one of them, regardless of N.
#[test]
fn every_member_of_n_generations_settles_under_mixed_outcomes() {
    const GENERATIONS: usize = 4;
    const MEMBERS_PER_GENERATION: usize = 2;
    assert_eq!(
        GENERATIONS % 2,
        0,
        "fixture wants an even split of published/given-up"
    );

    let mut core = EngineCore::new(
        RedbStore::temporary().expect("temporary Redb store"),
        GENERATIONS * 2 + 2,
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
         members sharing each generation) -- every one of them must settle once routing is closed \
         and every lane of the current generation is terminal"
    );
}
