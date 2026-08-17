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
//! write doors (`nmp-nip02`'s `writes.rs`, `nmp-nip29`'s
//! `group_list_writes.rs`, both moved out of this crate by #1707) both
//! unconditionally constructed -- could never
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
//! the real NIP-02/NIP-29 write doors (`nmp-nip02`'s `writes` module,
//! `nmp-nip29`'s `group_list_writes` module) because this reducer-level test
//! must not depend on either (#1707 moved both write doors out of `nmp`
//! entirely, and this crate sits BELOW `nmp`, so neither door is nameable
//! from here at all) and must run regardless of which protocol features are
//! enabled. The defect lived in the generic mechanism every semantic
//! capability shares, not in either protocol module's tag/kind logic.
//!
//! The fixture also has to answer #1631's per-relay coordinate question
//! before any delta reaches the wire, because a lane that never learns the
//! relay's current value for the coordinate is correctly parked rather than
//! sending a list built over a base that relay may already have superseded.
//!
//! nmp:falsifier=Every member receipt of every generation settles once its
//! lane is terminal, regardless of whether its relay published or gave up,
//! and no durable/in-memory semantic state survives the last terminal lane.
//!
//! #1406's own two remaining requirements -- retention and exact-generation
//! settlement -- are proven below by
//! [`settled_generations_leave_no_durable_state_and_are_not_resurrected`] and
//! [`a_stale_ack_for_a_superseded_generation_cannot_settle_its_successor`].
//! Both were verified non-vacuous by deliberately restoring the mistake they
//! name and confirming the assertion goes red, then reverting. The
//! exact-generation falsifier is notable: it took disabling FOUR independent
//! guards simultaneously (the reducer's `event_to_receipts` remap at both
//! accept-time and install-time, the reducer's exact lane-key match in
//! `handle_write_ack`, and the store's own CAS in both
//! `finish_lane_attempt` and `replace_lane_in_txn`) before a stale
//! predecessor ack could wrongly settle a successor generation -- defeating
//! any three of the four left it correctly inert. That defense-in-depth is
//! itself evidence worth recording, not just the passing assertion.

use super::*;
use nmp_grammar::{
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
    let spec = nmp_grammar::ReplaceableMaterializerSpec::new(
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
    coordinate: nostr::nips::nip01::Coordinate,
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
    arm_generation_with_author(core, registration, slot, member_count, &Keys::generate())
}

/// Same as [`arm_generation`], but with a caller-supplied author so a second
/// operation can be issued against the SAME coordinate later.
fn arm_generation_with_author(
    core: &mut EngineCore,
    registration: &RegisteredReplaceableMaterializer,
    slot: u32,
    member_count: usize,
    author: &Keys,
) -> ArmedGeneration {
    assert!(member_count >= 1);
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
        let signed = unsigned.sign_with_keys(author).expect("sign fixture");
        core.handle(EngineMsg::SignerCompleted(sign_id, generation, Ok(signed)));
        members.push(MemberReceipt { receipt });
    }

    let session = session_for(&relay, author);
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
    let scheduled = core.white_box("answer_coordinate_coverage_for_test", |s| {
        s.answer_coordinate_coverage_for_test(&[(read_handle, read_session)], &parked)
    });
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
        coordinate: nostr::nips::nip01::Coordinate {
            kind: Kind::ContactList,
            public_key: author.public_key(),
            identifier: String::new(),
        },
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

/// #1406's exact-generation falsifier: an `OK` naming a PREDECESSOR
/// generation's event must never terminalize (settle, mark `Published`, or
/// otherwise advance) its SUCCESSOR generation's members, even though both
/// generations share the same coordinate, the same author, the same relay
/// and session, and even the same owner receipt.
///
/// Sequence: generation 1 (E1) is signed and dispatched to its one relay
/// (`AwaitingAck`, no ack yet) -- then, while it is still in flight, a
/// SECOND operation on the exact same coordinate/author arrives. Because
/// generation 1's operation is still unresolved, this is a real
/// rematerialization (`plan_rematerialize`): the two engine-level guards
/// that make this safe are the reducer's own `event_to_receipts` remap
/// (`write.rs`'s `complete_replaceable_materialization`, which moves each
/// member's tracked event id from E1 to E2 the moment the new candidate is
/// installed) and the store's own predecessor-lane deletion
/// (`nmp-store/redb_store/semantic_edit_ops.rs`, which hard-deletes E1's
/// lane row in the SAME transaction that installs generation 2, and treats
/// a mismatched lane as a durable invariant violation). A late/duplicate OK
/// for E1 is delivered THREE times across the sequence -- before generation
/// 2 is even signed, after it is signed and dispatched, and interleaved with
/// generation 2's own real settlement -- and must be inert every time.
///
/// nmp:falsifier=An OK for a superseded generation's event can never settle,
/// publish, or otherwise advance its successor generation, even sharing the
/// same coordinate, author, relay, session and owner receipt.
#[test]
fn a_stale_ack_for_a_superseded_generation_cannot_settle_its_successor() {
    let author = Keys::generate();
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 8)
        .with_max_publish_attempts(1);
    let registration = install_capability(&mut core, [7u8; 16]);
    let gen1 = arm_generation_with_author(&mut core, &registration, 0, 1, &author);
    let gen1_receipt = gen1.members[0].receipt;

    // Generation 2: a second operation on the SAME coordinate/author while
    // generation 1 is still unresolved and in flight.
    let payload2 = registration
        .first_value_operation(Kind::ContactList, String::new(), vec![9, 9, 9])
        .expect("second first-value operation builds");
    let accepted2 = core.handle(EngineMsg::Publish(WriteIntent {
        payload: payload2,
        routing: WriteRouting::Explicit(vec![gen1.relay.clone()]),
        identity: Identity::Explicit(gen1.coordinate.public_key),
    }));
    let gen2_receipt = accepted2
        .iter()
        .find_map(|effect| match effect {
            Effect::WriteAccepted(id, _) => Some(*id),
            _ => None,
        })
        .expect("second write is accepted");
    let (sign_id, generation, unsigned) = accepted2
        .iter()
        .find_map(|effect| match effect {
            Effect::RequestSign(id, generation, unsigned) => {
                Some((*id, *generation, unsigned.clone()))
            }
            _ => None,
        })
        .expect("second accepted write requests signing");

    // Deliver the stale E1 OK again: generation 2 has been accepted (E1 is
    // already superseded in the resource row) but not yet signed.
    let stale_before_sign = core.handle(EngineMsg::RelayFrame(
        gen1.handle,
        gen1.session.clone(),
        RelayFrame::from(RelayMessage::ok(gen1.event_id, true, "")),
    ));
    assert!(
        stale_before_sign.is_empty(),
        "#1406: a stale E1 ack produced effects after E1 was superseded but before E2 was \
         signed -- got {stale_before_sign:?}"
    );

    let signed2 = unsigned.sign_with_keys(&author).expect("sign gen2");
    let event2_id = signed2.id;
    assert_ne!(
        event2_id, gen1.event_id,
        "gen2 must materialize a genuinely new event"
    );
    let complete2 = core.handle(EngineMsg::SignerCompleted(sign_id, generation, Ok(signed2)));

    let read_session = RelaySessionKey::public(gen1.relay.clone());
    let read_handle = TransportRelayHandle {
        slot: 1,
        generation: 1,
    };
    let scheduled = core.white_box("answer_coordinate_coverage_for_test", |s| {
        s.answer_coordinate_coverage_for_test(&[(read_handle, read_session)], &complete2)
    });
    let (correlation2, session2, dispatched_event2) = scheduled
        .iter()
        .find_map(|effect| match effect {
            Effect::PublishEvent(session, event, correlation) => {
                Some((*correlation, session.clone(), event.id))
            }
            _ => None,
        })
        .expect("gen2 dispatches");
    assert_eq!(dispatched_event2, event2_id);
    core.handle(EngineMsg::EventHandoff(
        correlation2,
        HandoffResult::Written,
    ));

    // Deliver the stale E1 OK a third time: generation 2 is now signed AND
    // dispatched (AwaitingAck of its own), sharing the exact same relay,
    // session and owner receipt E1 once used.
    let stale_after_dispatch = core.handle(EngineMsg::RelayFrame(
        gen1.handle,
        gen1.session.clone(),
        RelayFrame::from(RelayMessage::ok(gen1.event_id, true, "")),
    ));
    assert!(
        stale_after_dispatch.is_empty(),
        "#1406: a stale E1 ack produced effects after generation 2 was signed and dispatched \
         -- got {stale_after_dispatch:?}"
    );
    assert!(
        core.store
            .replaceable_operation_snapshot(&gen1.coordinate)
            .expect("store remains readable")
            .is_some(),
        "fixture sanity: the coordinate must still be open (gen2 unresolved) after the stale ack"
    );

    // Now deliver generation 2's REAL ack. Both the original member (still
    // carrying receipt id 1, now the owner of generation 2) and the new
    // member must settle -- and only once, for the right event.
    let real = core.handle(EngineMsg::RelayFrame(
        gen1.handle,
        session2.clone(),
        RelayFrame::from(RelayMessage::ok(event2_id, true, "")),
    ));
    let settled = settled_receipts(&real);
    assert_eq!(
        settled,
        BTreeSet::from([gen1_receipt, gen2_receipt]),
        "#1406: generation 2's real ack must settle exactly its own two members, no more and \
         no fewer -- got {settled:?}"
    );
    assert!(
        real.iter().any(|effect| matches!(
            effect,
            Effect::EmitReceipt(
                id,
                WriteFact::Relay { event_id, state: RelayState::Published, .. }
            ) if *id == gen1_receipt && *event_id == event2_id
        )),
        "the settled owner receipt must report Published against E2, never E1"
    );

    // Retention: once generation 2 settles, the coordinate's semantic state
    // -- including whatever generation 1 left behind -- is gone, and the
    // whole store's publish queue is empty.
    assert!(
        core.store
            .replaceable_operation_snapshot(&gen1.coordinate)
            .expect("store remains readable")
            .is_none(),
        "#1406: generation 2's settlement must retire the coordinate's semantic state entirely"
    );
    assert!(
        core.store
            .recover_publish_queue()
            .expect("store remains readable")
            .is_empty(),
        "#1406: generation 1's superseded lane/intent state must not survive generation 2's close"
    );

    // A late DUPLICATE of E1's ack, delivered after everything has closed,
    // must also remain inert -- no resurrection, no panic.
    let after_close = core.handle(EngineMsg::RelayFrame(
        gen1.handle,
        gen1.session.clone(),
        RelayFrame::from(RelayMessage::ok(gen1.event_id, true, "")),
    ));
    assert!(
        after_close.is_empty(),
        "#1406: a duplicate stale E1 ack after full settlement must remain a no-op -- got \
         {after_close:?}"
    );
}

/// #1406's retention falsifier: settlement firing (proven above) is not the
/// same claim as nothing surviving it. `close_cohort`
/// (`nmp-store/redb_store/semantic_edit_ops.rs`) deletes `SEMANTIC_RESOURCES`,
/// `SEMANTIC_OPERATIONS`, the member `publish_queue_intents` rows and their
/// retry deadlines in the same transaction that settles every receipt -- but
/// only when the reducer actually reaches it. This drives N real generations
/// (mixed `Published`/`GaveUp`, exactly like the settlement test above) to
/// completion through the real `EngineCore::handle` reducer path -- never by
/// calling `close_cohort`/`close_replaceable_operation_cohort` directly and
/// never by hand-writing resource-table state into a fixture -- and then
/// reads the store back to confirm each coordinate's semantic state is
/// actually gone, that the store-wide publish queue is empty (no leaked
/// intents or deadlines anywhere, not merely for the coordinates this test
/// happens to check), and that a later, wholly unrelated generation on a
/// fresh coordinate neither disturbs the retired ones nor leaves anything of
/// its own behind once it, too, settles.
///
/// nmp:falsifier=Once every member of a generation settles, its
/// `SEMANTIC_RESOURCES`/`SEMANTIC_OPERATIONS`/intent/deadline rows are gone,
/// the effect does not scale with how many generations preceded it, and nothing
/// later resurrects a retired coordinate's state.
#[test]
fn settled_generations_leave_no_durable_state_and_are_not_resurrected() {
    const GENERATIONS: usize = 3;

    let mut core = EngineCore::new(
        RedbStore::temporary().expect("temporary Redb store"),
        GENERATIONS * 2 + 4,
    )
    .with_max_publish_attempts(1);
    let registration = install_capability(&mut core, [6u8; 16]);

    let mut coordinates = Vec::with_capacity(GENERATIONS);
    for slot in 0..GENERATIONS as u32 {
        let gen = arm_generation(&mut core, &registration, slot, 2);
        let terminal_effects = if slot % 2 == 0 {
            core.handle(EngineMsg::RelayFrame(
                gen.handle,
                gen.session.clone(),
                RelayFrame::from(RelayMessage::ok(gen.event_id, true, "")),
            ))
        } else {
            core.handle(EngineMsg::RelayDisconnected(
                gen.handle,
                gen.session.clone(),
                DisconnectReason::Error,
            ))
        };
        let settled = settled_receipts(&terminal_effects);
        let expected: BTreeSet<_> = gen.members.iter().map(|m| m.receipt).collect();
        assert_eq!(
            settled, expected,
            "fixture sanity: generation {slot} must settle before its retention is meaningful"
        );
        coordinates.push(gen.coordinate);
    }

    for (slot, coordinate) in coordinates.iter().enumerate() {
        assert!(
            core.store
                .replaceable_operation_snapshot(coordinate)
                .expect("store remains readable")
                .is_none(),
            "#1406: generation {slot}'s coordinate still carries semantic resource/operation \
             state after every member settled"
        );
    }
    assert!(
        core.store
            .recover_publish_queue()
            .expect("store remains readable")
            .is_empty(),
        "#1406: {GENERATIONS} fully settled generations left publish-queue intents or \
         deadlines behind -- retention does not scale with how many preceded it, it is zero"
    );

    // A later, wholly unrelated generation must neither resurrect any retired
    // coordinate above nor, once it settles in turn, leave anything of its
    // own behind either.
    let unrelated = arm_generation(&mut core, &registration, GENERATIONS as u32, 1);
    let unrelated_effects = core.handle(EngineMsg::RelayFrame(
        unrelated.handle,
        unrelated.session.clone(),
        RelayFrame::from(RelayMessage::ok(unrelated.event_id, true, "")),
    ));
    assert_eq!(
        settled_receipts(&unrelated_effects),
        unrelated.members.iter().map(|m| m.receipt).collect(),
        "fixture sanity: the later unrelated generation must itself settle"
    );

    for (slot, coordinate) in coordinates.iter().enumerate() {
        assert!(
            core.store
                .replaceable_operation_snapshot(coordinate)
                .expect("store remains readable")
                .is_none(),
            "#1406: an unrelated later write resurrected retired generation {slot}'s \
             semantic state"
        );
    }
    assert!(
        core.store
            .replaceable_operation_snapshot(&unrelated.coordinate)
            .expect("store remains readable")
            .is_none(),
        "#1406: the later generation's own coordinate must also be retired once it settles"
    );
    assert!(
        core.store
            .recover_publish_queue()
            .expect("store remains readable")
            .is_empty(),
        "#1406: the later unrelated generation left its own publish-queue state behind"
    );
}
