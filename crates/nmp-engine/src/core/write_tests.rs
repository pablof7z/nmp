//! Ownership-domain tests moved with the implementation they falsify.

use super::*;

#[cfg(test)]
mod receipt_allocator_tests {
    use super::*;

    use nmp_router_testkit::FixtureRoutingFacts;
    use nmp_store::{PersistenceFault, RedbStore};
    use nostr::{EventBuilder, Keys, Kind};

    /// The same frozen note, `p`-tagging the given recipients — the shape
    /// whose route answer has more than one contributor to wait on.
    fn frozen_note_mentioning(author: PublicKey, recipients: &[PublicKey]) -> SignedEvent {
        let created_at = Timestamp::from(1_700_000_000);
        let kind = Kind::TextNote;
        let tags = nostr::Tags::from_list(
            recipients
                .iter()
                .map(|r| nostr::Tag::public_key(*r))
                .collect(),
        );
        let content = "route need".to_string();
        SignedEvent::new(
            EventId::new(&author, &created_at, &kind, &tags, &content),
            author,
            created_at,
            kind,
            tags,
            content,
            nmp_store::sentinel_signature(),
        )
    }

    fn frozen_note(author: PublicKey) -> SignedEvent {
        let created_at = Timestamp::from(1_700_000_000);
        let kind = Kind::TextNote;
        let tags = nostr::Tags::new();
        let content = "route need".to_string();
        SignedEvent::new(
            EventId::new(&author, &created_at, &kind, &tags, &content),
            author,
            created_at,
            kind,
            tags,
            content,
            nmp_store::sentinel_signature(),
        )
    }

    #[test]
    fn acceptance_io_refuses_publish_and_requests_store_reconstruction() {
        let persistent_fixture = tempfile::tempdir().expect("persistent store fixture");
        let store = RedbStore::open_with_accept_write_precommit_io(
            persistent_fixture
                .path()
                .join("core-precommit-acceptance.redb"),
        )
        .expect("persistent precommit-I/O store must open");
        let mut core = EngineCore::new(store, 10);
        let author = Keys::generate();
        core.handle(EngineMsg::SetActivePubkey(Some(author.public_key())));
        let event = EventBuilder::text_note("acceptance I/O")
            .sign_with_keys(&author)
            .unwrap();

        let effects = core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::Signed(event),
            routing: WriteRouting::Explicit(vec![
                RelayUrl::parse("wss://acceptance-io.example").unwrap()
            ]),
            identity: Identity::Active,
            correlation: None,
        }));

        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::PublishFailed(PublishError::PersistenceFailed { reason })
                if reason.contains("injected acceptance failed before commit")
        )));
        assert_eq!(
            core.take_store_recovery_request(),
            Some(PersistenceFault::Io),
            "acceptance I/O must arm concrete-store reconstruction"
        );
    }

    #[test]
    fn invariant_store_failure_does_not_request_reconstruction() {
        let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 10);
        let mut effects = Vec::new();

        core.degrade_store(
            nmp_store::PersistenceError::invariant("fixture invariant"),
            &mut effects,
        );
        assert_eq!(
            core.take_store_recovery_request(),
            None,
            "a healthy handle cannot be made safer by reconstruction"
        );

        core.degrade_store(
            nmp_store::PersistenceError::new(nmp_store::PersistenceFault::Io, "fixture I/O"),
            &mut effects,
        );
        assert!(
            matches!(
                effects.last(),
                Some(Effect::EmitDiagnostics(snapshot))
                    if snapshot.store_degraded.as_deref()
                        == Some("durable-store persistence failure: fixture invariant")
            ),
            "a later distinct I/O failure must not replace the first diagnostic"
        );
        assert_eq!(
            core.take_store_recovery_request(),
            Some(nmp_store::PersistenceFault::Io),
            "the same typed branch must still arm reconstruction for I/O"
        );
    }

    #[test]
    fn last_attempt_correlation_is_issued_once_then_exhaustion_is_stable_and_typed() {
        let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 10);
        core.set_next_attempt_correlation_for_test(Some(u64::MAX));

        assert_eq!(
            core.alloc_attempt_correlation(),
            Ok(AttemptCorrelation(u64::MAX))
        );
        assert_eq!(
            core.alloc_attempt_correlation(),
            Err(AttemptCorrelationExhausted)
        );
        assert_eq!(
            core.alloc_attempt_correlation(),
            Err(AttemptCorrelationExhausted),
            "exhaustion remains stable: no wrap, reuse, or fabricated id"
        );
    }

    #[test]
    fn attempt_correlation_exhaustion_precedes_lane_and_pending_mutation() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://correlation-exhausted.example").unwrap();
        let directory =
            FixtureRoutingFacts::new().with_outbound_routes(keys.public_key(), [relay.clone()]);
        let mut core = EngineCore::new_with_fixture_routing_facts(
            RedbStore::temporary().expect("temporary Redb store"),
            directory,
            10,
        );
        core.handle(EngineMsg::SetActivePubkey(Some(keys.public_key())));
        let accepted = core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::Event(nmp_grammar::EventBuilder {
                kind: Kind::TextNote,
                tags: (vec![]).into_iter().collect(),
                content: ("correlation boundary").into(),
                created_at: Some(Timestamp::from(93u64)),
            }),
            routing: WriteRouting::Auto,
            identity: Identity::Active,
            correlation: None,
        }));
        let (receipt, generation, unsigned) = accepted
            .iter()
            .find_map(|effect| match effect {
                Effect::RequestSign(receipt, generation, unsigned) => {
                    Some((*receipt, *generation, unsigned.clone()))
                }
                _ => None,
            })
            .expect("accepted unsigned intent requests signing");
        let intent = core.pending[&receipt].intent_id;
        core.set_next_attempt_correlation_for_test(None);

        let effects = core.handle(EngineMsg::SignerCompleted(
            receipt,
            generation,
            Ok(unsigned.sign_with_keys(&keys).unwrap()),
        ));

        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, Effect::PublishEvent(..))));
        assert!(core.attempt_correlations.is_empty());
        assert!(core.pending[&receipt].pending_relays.is_empty());
        assert!(core.pending[&receipt].attempt_ordinals.is_empty());
        assert!(core.store.recover_attempts(intent).unwrap().is_empty());
        assert_eq!(
            core.alloc_attempt_correlation(),
            Err(AttemptCorrelationExhausted),
            "the failed call must not revive or wrap the namespace"
        );
    }

    /// Verbatim execution: an explicit route resolves to exactly the relays
    /// the caller named. The directory is deliberately populated with a
    /// DIFFERENT write relay for this very author -- if resolution consulted
    /// it at all, that relay would show up here.
    #[test]
    fn an_explicit_route_resolves_verbatim_and_never_consults_the_directory() {
        let author = Keys::generate().public_key();
        let a = RelayUrl::parse("wss://chosen-a.example").unwrap();
        let b = RelayUrl::parse("wss://chosen-b.example").unwrap();
        let unrelated = RelayUrl::parse("wss://unrelated.example").unwrap();
        let directory = FixtureRoutingFacts::new().with_outbound_routes(author, [unrelated]);
        let core = EngineCore::new_with_fixture_routing_facts(
            RedbStore::temporary().expect("temporary Redb store"),
            directory,
            10,
        );
        let route = WriteRouting::Explicit(vec![b.clone(), a.clone()]);

        let created_at = Timestamp::from(1_700_000_000);
        let kind = nostr::Kind::TextNote;
        let tags = nostr::Tags::new();
        let content = "explicit route".to_string();
        let frozen = SignedEvent::new(
            EventId::new(&author, &created_at, &kind, &tags, &content),
            author,
            created_at,
            kind,
            tags,
            content,
            nmp_store::sentinel_signature(),
        );

        let answer = core.resolve_routes(&route, &frozen).answer;
        assert_eq!(
            answer.relays,
            BTreeSet::from([a, b]),
            "an explicit route executes only the caller's set and never unions a directory fact"
        );
        assert!(
            answer.complete && answer.author_route_needs.is_empty(),
            "verbatim execution reads nothing, so it can never learn anything later: {answer:?}"
        );
    }

    #[test]
    fn a_zero_destination_answer_separates_still_looking_from_finished_looking() {
        // The whole of #1236, as a type rather than a string. `complete` is a
        // statement about KNOWLEDGE EXHAUSTION, never about delivery, and a
        // zero-destination answer is where that earns its keep: an app must
        // show "determining destinations" for one and "nowhere to publish"
        // for the other, and collapsing them makes both unactionable.
        let author = Keys::generate().public_key();
        let event = frozen_note(author);

        // Still looking: nobody has finished the lookup, so nothing has been
        // learned yet and NOTHING may expire this.
        let core = EngineCore::new_with_fixture_routing_facts(
            RedbStore::temporary().expect("temporary Redb store"),
            FixtureRoutingFacts::new(),
            10,
        );
        let unknown = core.resolve_routes(&WriteRouting::Auto, &event).answer;
        assert!(
            unknown.relays.is_empty() && !unknown.complete,
            "an unsettled author must park, not terminate: {unknown:?}"
        );
        assert_eq!(
            unknown.author_route_needs,
            BTreeSet::from([author]),
            "a parked write keeps its contributor declared, because a later positive replacement is the only unpark signal"
        );

        // Finished looking, twice over. A settled-but-empty list and a
        // settled absence are both answers, and an answer that names nobody
        // means there is nowhere to publish.
        for (label, facts) in [
            (
                "Present empty",
                FixtureRoutingFacts::new().with_author_routes(author, [], []),
            ),
            (
                "Absent",
                FixtureRoutingFacts::new().with_author_absent(author),
            ),
        ] {
            let core = EngineCore::new_with_fixture_routing_facts(
                RedbStore::temporary().expect("temporary Redb store"),
                facts,
                10,
            );
            let answer = core.resolve_routes(&WriteRouting::Auto, &event).answer;

            assert!(
                answer.relays.is_empty() && answer.complete,
                "{label} with no operator route is knowledge EXHAUSTED, so the write has nowhere to go rather than something to wait for: {answer:?}"
            );
            assert!(
                answer.author_route_needs.is_empty(),
                "{label} has nothing left to look up, so it must not hold a provider open on a question already answered: {answer:?}"
            );
        }
    }

    /// A park can NARROW without its relay set or its completeness moving,
    /// and when it does, the waiting set is the only thing that carries the
    /// news.
    ///
    /// This is what makes the waiting set load-bearing in `rewrite_route`'s
    /// `picture_changed`. One of two unlooked-up recipients settling is a
    /// real change an app can act on — one fewer person to chase — and both
    /// of the other two axes are byte-identical across it. A receipt that
    /// compared only `relays` and `complete` would report the FIRST reason
    /// and then go silent while the reason changed underneath the app, which
    /// is the same "stuck, and you cannot tell why" defect #1236 is about.
    #[test]
    fn a_narrowing_park_moves_only_the_waiting_set() {
        let author = Keys::generate().public_key();
        let outbox = RelayUrl::parse("wss://outbox-a.example").unwrap();
        let settling = Keys::generate().public_key();
        let staying = Keys::generate().public_key();
        let event = frozen_note_mentioning(author, &[settling, staying]);

        // Both recipients unlooked-up: the answer waits on both.
        let before = EngineCore::new_with_fixture_routing_facts(
            RedbStore::temporary().expect("temporary Redb store"),
            FixtureRoutingFacts::new().with_outbound_routes(author, [outbox.clone()]),
            10,
        )
        .resolve_routes(&WriteRouting::Auto, &event)
        .answer;

        // One of them settles as a definitive absence. It contributes no
        // relay, so the destination set cannot move.
        let after = EngineCore::new_with_fixture_routing_facts(
            RedbStore::temporary().expect("temporary Redb store"),
            FixtureRoutingFacts::new()
                .with_outbound_routes(author, [outbox.clone()])
                .with_author_absent(settling),
            10,
        )
        .resolve_routes(&WriteRouting::Auto, &event)
        .answer;

        assert_eq!(
            before.relays, after.relays,
            "the settling recipient names no relay, so the destination set must not move: \
             {before:?} -> {after:?}"
        );
        assert_eq!(
            (before.complete, after.complete),
            (false, false),
            "one recipient settling does not exhaust knowledge while the other is unknown: \
             {before:?} -> {after:?}"
        );
        assert_eq!(
            before.author_route_needs,
            BTreeSet::from([settling, staying]),
            "an open answer waits on every unlooked-up recipient: {before:?}"
        );
        assert_eq!(
            after.author_route_needs,
            BTreeSet::from([staying]),
            "a settled recipient must leave the waiting set, or the app keeps chasing an \
             answer that already arrived: {after:?}"
        );
        assert_ne!(
            before.author_route_needs, after.author_route_needs,
            "the waiting set is the ONLY axis carrying this change, which is why the receipt \
             must compare it before deciding the picture is unchanged"
        );
    }

    #[test]
    fn a_settled_zero_route_author_retires_when_an_operator_route_exists() {
        let author = Keys::generate().public_key();
        let app = RelayUrl::parse("wss://app.example").unwrap();
        let facts = FixtureRoutingFacts::new()
            .with_author_absent(author)
            .with_operator_app([app.clone()]);
        let core = EngineCore::new_with_fixture_routing_facts(
            RedbStore::temporary().expect("temporary Redb store"),
            facts,
            10,
        );

        let answer = core
            .resolve_routes(&WriteRouting::Auto, &frozen_note(author))
            .answer;

        assert_eq!(answer.relays, BTreeSet::from([app]));
        assert!(
            answer.complete && answer.author_route_needs.is_empty(),
            "a settled contributor with an actual destination has no remaining provider need: {answer:?}"
        );
    }

    /// The durable value is a STRATEGY. `Auto` journals a bare label with no
    /// relay in it, so replaying it after a crash re-resolves against
    /// whatever the engine knows then rather than against a stale answer;
    /// `Explicit` journals exactly the relays it was given, in order.
    #[test]
    fn routing_snapshots_round_trip_the_strategy_not_a_resolved_relay_set() {
        let a = RelayUrl::parse("wss://chosen-a.example").unwrap();
        let b = RelayUrl::parse("wss://chosen-b.example").unwrap();

        let auto = EngineCore::routing_snapshot(&WriteRouting::Auto);
        assert_eq!(auto, "auto", "Auto stores a label, never a relay set");
        assert!(matches!(
            EngineCore::parse_routing_snapshot(&auto),
            Some(WriteRouting::Auto)
        ));

        let route = WriteRouting::Explicit(vec![b.clone(), a.clone()]);
        let snapshot = EngineCore::routing_snapshot(&route);
        let restored = EngineCore::parse_routing_snapshot(&snapshot)
            .expect("a valid explicit snapshot must remain readable");
        let WriteRouting::Explicit(relays) = restored else {
            panic!("snapshot restored the wrong routing variant")
        };
        assert_eq!(relays, vec![b, a]);
    }

    #[test]
    fn boot_redeclares_recovered_auto_route_needs_to_protocol_assembly() {
        let keys = Keys::generate();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("route-needs.redb");

        {
            let mut core = EngineCore::new(RedbStore::open(&path).unwrap(), 10);
            core.handle(EngineMsg::SetActivePubkey(Some(keys.public_key())));
            let accepted = core.handle(EngineMsg::Publish(WriteIntent {
                payload: WritePayload::Event(nmp_grammar::EventBuilder {
                    kind: Kind::TextNote,
                    tags: (vec![]).into_iter().collect(),
                    content: ("recover my route need").into(),
                    created_at: Some(Timestamp::from(100u64)),
                }),
                routing: WriteRouting::Auto,
                identity: Identity::Active,
                correlation: None,
            }));
            let (receipt, generation, unsigned) = accepted
                .iter()
                .find_map(|effect| match effect {
                    Effect::RequestSign(receipt, generation, unsigned) => {
                        Some((*receipt, *generation, unsigned.clone()))
                    }
                    _ => None,
                })
                .expect("accepted auto write requests signing");
            core.handle(EngineMsg::SignerCompleted(
                receipt,
                generation,
                Ok(unsigned.sign_with_keys(&keys).unwrap()),
            ));
            assert_eq!(
                core.author_route_needs(),
                BTreeSet::from([keys.public_key()])
            );
        }

        let mut recovered = EngineCore::new(RedbStore::open(&path).unwrap(), 10);
        let effects = recovered.recover_on_boot();
        let replayed = effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::AuthorRouteNeedsChanged(needs) => Some(needs.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(
            replayed,
            vec![BTreeSet::from([keys.public_key()])],
            "boot must publish the exact stateless route-need set rebuilt from durable intents"
        );

        let mut immediate_resync = Vec::new();
        recovered.resync_route_needs(&mut immediate_resync);
        assert!(
            immediate_resync
                .iter()
                .all(|effect| !matches!(effect, Effect::AuthorRouteNeedsChanged(_))),
            "boot recovery must update the same edge cache as live synchronization, or the first unrelated recompile repeats the recovered need set"
        );
    }
}

#[cfg(test)]
mod semantic_successor_tests {
    use super::*;
    use nmp_grammar::{
        ReplaceableMaterializer, ReplaceableMaterializerOperation, ReplaceableMaterializerRefusal,
        ReplaceableMaterializerRegistration,
    };
    use nmp_store::{RedbStore, RelayObserved};
    use nostr::nips::nip01::Coordinate;
    use nostr::{EventBuilder, Keys, Kind, Tag};
    use std::sync::Arc;

    struct AddPeople;

    impl ReplaceableMaterializer for AddPeople {
        fn materialize(
            &self,
            _source: &UnsignedEvent,
            current: &UnsignedEvent,
            operations: &[ReplaceableMaterializerOperation<'_>],
        ) -> Result<nmp_grammar::EventBuilder, ReplaceableMaterializerRefusal> {
            let mut tags = current.tags.clone().to_vec();
            for operation in operations {
                let key = PublicKey::from_slice(operation.bytes()).map_err(|error| {
                    ReplaceableMaterializerRefusal {
                        reason: error.to_string(),
                    }
                })?;
                if !tags
                    .iter()
                    .any(|tag| tag.as_slice() == ["p", &key.to_hex()])
                {
                    tags.push(Tag::public_key(key));
                }
            }
            Ok(nmp_grammar::EventBuilder {
                kind: current.kind,
                tags,
                content: current.content.clone(),
                created_at: None,
            })
        }

        fn materialize_default(
            &self,
            coordinate: &Coordinate,
            operations: &[ReplaceableMaterializerOperation<'_>],
        ) -> Result<nmp_grammar::EventBuilder, ReplaceableMaterializerRefusal> {
            let mut tags = if coordinate.kind.is_addressable() {
                vec![Tag::identifier(coordinate.identifier.clone())]
            } else {
                Vec::new()
            };
            for operation in operations {
                let key = PublicKey::from_slice(operation.bytes()).map_err(|error| {
                    ReplaceableMaterializerRefusal {
                        reason: error.to_string(),
                    }
                })?;
                if !tags
                    .iter()
                    .any(|tag| tag.as_slice() == ["p", &key.to_hex()])
                {
                    tags.push(Tag::public_key(key));
                }
            }
            Ok(nmp_grammar::EventBuilder {
                kind: coordinate.kind,
                tags,
                content: String::new(),
                created_at: None,
            })
        }
    }

    fn source(keys: &Keys, at: u64, content: &str, people: &[PublicKey]) -> SignedEvent {
        EventBuilder::new(Kind::ContactList, content)
            .tags(people.iter().copied().map(Tag::public_key))
            .custom_created_at(Timestamp::from(at))
            .sign_with_keys(keys)
            .unwrap()
    }

    /// #1683: a coordinate whose only request on a relay is NIP-77's
    /// live-first `limit: 0` barrier must not let the publish gate send.
    ///
    /// The barrier asks for no stored event, so it answers nothing on its own
    /// -- but the question HAS been asked, and reconciliation is what answers
    /// it. Before that state was nameable the gate saw `Uncovered`, could not
    /// tell it from "nothing ever asked", and published a delta built on a
    /// base this relay may already have superseded. That loss is terminal:
    /// the relay then serves NMP's value and the newer list it held is gone.
    ///
    /// Driven through the real publish path -- prepare, sign, connect,
    /// admission flush -- and never by calling the coverage door, because the
    /// defect was the GATE's reading of the door and a door-level test cannot
    /// see it.
    #[test]
    fn a_live_first_barrier_never_publishes_over_an_unread_base() {
        let author = Keys::generate();
        let existing_person = Keys::generate().public_key();
        let added_person = Keys::generate().public_key();
        let source_relay = RelayUrl::parse("wss://barrier-source.example").unwrap();
        let destination = RelayUrl::parse("wss://barrier-destination.example").unwrap();
        let mut store = RedbStore::temporary().expect("temporary Redb store");
        store
            .insert(
                source(&author, 1, "base body", &[existing_person]),
                RelayObserved::new(source_relay.clone(), Timestamp::from(1)),
            )
            .unwrap();
        let mut core = EngineCore::new(store, 10);
        core.handle(EngineMsg::SetActivePubkey(Some(author.public_key())));
        core.install_replaceable_materializer(ReplaceableMaterializerRegistration {
            program: [11; 16],
            format: [12; 16],
            materializer: Arc::new(AddPeople),
        });
        // The destination speaks NIP-77, so the coordinate read this gate
        // opens compiles to a live-first barrier rather than an ordinary REQ.
        core.prober.states.insert(
            destination.clone(),
            crate::negentropy::ProbeState::Supported,
        );

        let operation = nmp_grammar::ReplaceableOperation::from_registered_default_parts(
            [11; 16],
            [12; 16],
            Kind::ContactList,
            String::new(),
            added_person.to_bytes().to_vec(),
        )
        .unwrap();
        let mut preparation = core.prepare_publish(WriteIntent {
            payload: WritePayload::ReplaceableOperation(operation),
            routing: WriteRouting::Explicit(vec![destination.clone()]),
            identity: Identity::Active,
            correlation: None,
        });
        let accepted = loop {
            match preparation {
                PublishPreparation::Complete(effects) => break effects,
                PublishPreparation::Materialize(prepared) => {
                    let PreparedReplaceableMaterialization { call, continuation } = *prepared;
                    let outcome = core.run_replaceable_materialization(call);
                    preparation =
                        core.complete_body_complete_replaceable_operation(continuation, outcome);
                }
            }
        };
        let (owner, generation, unsigned) = accepted
            .iter()
            .find_map(|effect| match effect {
                Effect::RequestSign(receipt, generation, unsigned) => {
                    Some((*receipt, *generation, unsigned.clone()))
                }
                _ => None,
            })
            .expect("the delta generation requests one signature");
        let signed = unsigned.sign_with_keys(&author).unwrap();
        core.handle(EngineMsg::SignerCompleted(
            owner,
            generation,
            Ok(signed.clone()),
        ));

        let coordinate = nostr::nips::nip01::Coordinate {
            kind: Kind::ContactList,
            public_key: author.public_key(),
            identifier: String::new(),
        };
        let read_session = RelaySessionKey::public(destination.clone());
        let read_handle = TransportRelayHandle {
            slot: 40,
            generation: 1,
        };
        let write_session = RelaySessionKey::new(
            destination.clone(),
            AccessContext::Nip42(author.public_key()),
        );
        let write_handle = TransportRelayHandle {
            slot: 1,
            generation: 1,
        };
        core.handle(EngineMsg::RelayConnected(read_handle, read_session.clone()));
        core.handle(EngineMsg::RelayConnected(
            write_handle,
            write_session.clone(),
        ));
        // Driven inline rather than through `answer_coordinate_coverage_for_test`
        // because reaching the window needs REPEATED flush/EOSE rounds and
        // that helper does exactly one. (It used to swallow its own flush too,
        // which is what made the first version of this test vacuous; that is
        // fixed, so capture is no longer the reason -- only the loop is.)
        let mut seen = core.handle(EngineMsg::AuthProbeReleased(write_handle, write_session));
        // Proof the scenario actually reaches the window this test is about,
        // recorded AS IT HAPPENS: a run that never gets here would otherwise
        // pass by never being tested at all.
        let mut window_entered = false;
        for _ in 0..6 {
            let next = Timestamp::from(core.clock.as_secs().saturating_add(1));
            let flushed = core.handle(EngineMsg::FlushWireAdmission(next));
            let mut sub_ids = Vec::new();
            for effect in flushed.iter() {
                let Effect::Wire(delta) = effect else {
                    continue;
                };
                for (candidate, ops) in &delta.ops {
                    if candidate != &read_session {
                        continue;
                    }
                    for op in ops {
                        let WireOp::Req(sub_id, filter) = op else {
                            continue;
                        };
                        let attempt_id = delta.attempt_id(candidate, sub_id, filter);
                        seen.extend(core.on_wire_request_handoff(
                            RequestHandoffOutcome::Accepted {
                                attempt_id,
                                handle: read_handle,
                            },
                        ));
                        sub_ids.push(sub_id.clone());
                    }
                }
            }
            seen.extend(flushed);
            window_entered |= !core.wire_admission_needed()
                && matches!(
                    core.coordinate_coverage(&coordinate, &read_session),
                    crate::core::coordinate_coverage::CoordinateCoverage::Reconciling { .. }
                );
            for sub_id in sub_ids {
                seen.extend(core.handle(EngineMsg::RelayFrame(
                    read_handle,
                    read_session.clone(),
                    RelayFrame::from_message(nostr::RelayMessage::EndOfStoredEvents(
                        std::borrow::Cow::Owned(nostr::SubscriptionId::new(wire_sub_id_string(
                            &sub_id,
                        ))),
                    )),
                )));
            }
        }

        // The window under test is the moment admission has nothing left
        // pending while the coordinate is still answered only by
        // reconciliation. Anything short of that parks for an unrelated
        // reason and would prove nothing.
        let published = seen.iter().any(|effect| {
            matches!(effect, Effect::PublishEvent(session, _, _) if session.relay == destination)
        });
        assert!(
            !published,
            "a coordinate answered only by a live-first barrier must not publish a delta \
             over a base this relay may have superseded"
        );
        // Non-vacuity, checked only once the property above holds: the run
        // must actually have reached a quiet-admission moment with the
        // coordinate answered only by reconciliation, or the assertion above
        // passed by never being tested.
        assert!(
            window_entered,
            "this test proves nothing unless the run reaches that window"
        );
    }

    /// #1683's residual cause, established: a covering coordinate REQ can
    /// finish with its coverage authority POISONED -- here, an EVENT it
    /// delivered failed to commit to the store -- so
    /// `persist_attributed_completion` retires it without a coverage
    /// interval (`Finished { committed_interval: None }`). A poisoned finish
    /// proves neither presence nor absence, and the coverage door's
    /// `Finished` arm only ever tries to prove absence, so it silently
    /// contributes nothing: indistinguishable from "nothing ever asked" to a
    /// caller reading `Uncovered`.
    ///
    /// This is a reachability/characterization proof, not a fix: two
    /// alternatives (retry the ask instead of sending; always park) were
    /// tried and rejected, because both deterministically stall
    /// `relay_source_successors_resume_current_delivery_and_stay_open_after_restart`
    /// and `source_session_replacement_wakes_every_signed_successor_destination`
    /// -- see the long comment on `coordinate_is_current_for_lane`'s
    /// `Uncovered` arm for why. Sending remains the deliberate, recorded
    /// choice (`docs/known-gaps.md`). This test proves the state is
    /// genuinely reachable through a real mechanism and confirms today's
    /// actual behavior, so nobody re-derives "not established" from scratch.
    ///
    /// Driven through the real publish path -- prepare, sign, connect,
    /// admission flush -- and never by calling the coverage door directly.
    /// The poison is injected the way a store commit failure would compile
    /// to it (`AttributionState::poison_event_commit_failure`, the same door
    /// `on_relay_frame` calls on a real ingest failure), not by asserting on
    /// `CoordinateCoverage` itself.
    #[test]
    fn a_poisoned_finished_coordinate_request_is_read_as_uncovered_and_the_lane_sends() {
        let author = Keys::generate();
        let existing_person = Keys::generate().public_key();
        let added_person = Keys::generate().public_key();
        let source_relay = RelayUrl::parse("wss://poison-source.example").unwrap();
        let destination = RelayUrl::parse("wss://poison-destination.example").unwrap();
        let mut store = RedbStore::temporary().expect("temporary Redb store");
        store
            .insert(
                source(&author, 1, "base body", &[existing_person]),
                RelayObserved::new(source_relay.clone(), Timestamp::from(1)),
            )
            .unwrap();
        let mut core = EngineCore::new(store, 10);
        core.handle(EngineMsg::SetActivePubkey(Some(author.public_key())));
        core.install_replaceable_materializer(ReplaceableMaterializerRegistration {
            program: [21; 16],
            format: [22; 16],
            materializer: Arc::new(AddPeople),
        });

        let read_session = RelaySessionKey::public(destination.clone());
        let read_handle = TransportRelayHandle {
            slot: 40,
            generation: 1,
        };
        let write_session = RelaySessionKey::new(
            destination.clone(),
            AccessContext::Nip42(author.public_key()),
        );
        let write_handle = TransportRelayHandle {
            slot: 1,
            generation: 1,
        };
        core.handle(EngineMsg::RelayConnected(read_handle, read_session.clone()));
        core.handle(EngineMsg::RelayConnected(
            write_handle,
            write_session.clone(),
        ));

        let mut seen: Vec<Effect> = Vec::new();

        let operation = nmp_grammar::ReplaceableOperation::from_registered_default_parts(
            [21; 16],
            [22; 16],
            Kind::ContactList,
            String::new(),
            added_person.to_bytes().to_vec(),
        )
        .unwrap();
        let mut preparation = core.prepare_publish(WriteIntent {
            payload: WritePayload::ReplaceableOperation(operation),
            routing: WriteRouting::Explicit(vec![destination.clone()]),
            identity: Identity::Active,
            correlation: None,
        });
        let accepted = loop {
            match preparation {
                PublishPreparation::Complete(effects) => break effects,
                PublishPreparation::Materialize(prepared) => {
                    let PreparedReplaceableMaterialization { call, continuation } = *prepared;
                    let outcome = core.run_replaceable_materialization(call);
                    preparation =
                        core.complete_body_complete_replaceable_operation(continuation, outcome);
                }
            }
        };
        let (owner, generation, unsigned) = accepted
            .iter()
            .find_map(|effect| match effect {
                Effect::RequestSign(receipt, generation, unsigned) => {
                    Some((*receipt, *generation, unsigned.clone()))
                }
                _ => None,
            })
            .expect("the delta generation requests one signature");
        let signed = unsigned.sign_with_keys(&author).unwrap();
        seen.extend(core.handle(EngineMsg::SignerCompleted(
            owner,
            generation,
            Ok(signed.clone()),
        )));
        seen.extend(core.handle(EngineMsg::AuthProbeReleased(write_handle, write_session)));

        // The coverage door's own coordinate REQ is now live. Accept its
        // handoff so it becomes a real, unlimited, unpoisoned wire owner --
        // exactly what `coordinate_filter` always asks for.
        let mut coordinate_sub_id = None;
        for round in 0..3 {
            if coordinate_sub_id.is_some() {
                break;
            }
            let next = Timestamp::from(core.clock.as_secs().saturating_add(1));
            let flushed = core.handle(EngineMsg::FlushWireAdmission(next));
            let mut round_accepts = Vec::new();
            for effect in &flushed {
                if let Effect::Wire(delta) = effect {
                    for (session, ops) in &delta.ops {
                        if session != &read_session {
                            continue;
                        }
                        for op in ops {
                            let WireOp::Req(sub_id, filter) = op else {
                                continue;
                            };
                            round_accepts.push(RequestHandoffOutcome::Accepted {
                                attempt_id: delta.attempt_id(session, sub_id, filter),
                                handle: read_handle,
                            });
                            coordinate_sub_id = Some(sub_id.clone());
                        }
                    }
                }
            }
            for accept in round_accepts {
                seen.extend(core.on_wire_request_handoff(accept));
            }
            seen.extend(flushed);
            let _ = round;
        }
        let coordinate_sub_id =
            coordinate_sub_id.expect("the coverage door places its own coordinate REQ");
        assert!(
            core.semantic_publish_coverage.values().next().is_some(),
            "the lane must already own an open coordinate observation before the poison lands"
        );

        // Poison this exact request's coverage authority the way a real
        // store commit failure would, then finish its stored-events phase.
        // It delivered nothing and proves nothing -- Finished, no interval.
        let wire_id = wire_sub_id_string(&coordinate_sub_id);
        core.attribution
            .poison_event_commit_failure(&read_session, &wire_id);
        seen.extend(core.handle(EngineMsg::RelayFrame(
            read_handle,
            read_session.clone(),
            RelayFrame::from_message(nostr::RelayMessage::EndOfStoredEvents(
                std::borrow::Cow::Owned(nostr::SubscriptionId::new(wire_id.clone())),
            )),
        )));

        let coordinate = nostr::nips::nip01::Coordinate {
            kind: Kind::ContactList,
            public_key: author.public_key(),
            identifier: String::new(),
        };
        assert_eq!(
            core.coordinate_coverage(&coordinate, &read_session),
            crate::core::coordinate_coverage::CoordinateCoverage::Uncovered,
            "a poisoned finish must be unable to prove absence, or this test proves nothing"
        );

        // Drain a few more admission passes: the wake-parked mechanism
        // (EngineCore::handle) re-runs `schedule_ready` on every turn while
        // this lane is parked, which is exactly where the escape fires.
        for _ in 0..3 {
            let next = Timestamp::from(core.clock.as_secs().saturating_add(1));
            seen.extend(core.handle(EngineMsg::FlushWireAdmission(next)));
        }

        // Today's actual, deliberate behavior: the lane sends rather than
        // parking forever. This is the residual #1683 records, not a
        // regression -- see the comment this test is cited from.
        let published = seen.iter().any(|effect| {
            matches!(effect, Effect::PublishEvent(session, _, _) if session.relay == destination)
        });
        assert!(
            published,
            "the poisoned-finished coordinate request must read as Uncovered and let this \
             already-asked lane send -- if this now fails, the escape changed and the comment \
             on coordinate_is_current_for_lane's Uncovered arm needs to change with it"
        );
    }

    #[test]
    fn capability_default_fallback_uses_a_preexisting_canonical_source() {
        let author = Keys::generate();
        let existing_person = Keys::generate().public_key();
        let added_person = Keys::generate().public_key();
        let source_relay = RelayUrl::parse("wss://known-source.example").unwrap();
        let destination = RelayUrl::parse("wss://known-source-destination.example").unwrap();
        let base = source(&author, 1, "known relay body", &[existing_person]);
        let mut store = RedbStore::temporary().expect("temporary Redb store");
        store
            .insert(
                base,
                RelayObserved::new(source_relay.clone(), Timestamp::from(1)),
            )
            .unwrap();
        let mut core = EngineCore::new(store, 10);
        core.handle(EngineMsg::SetActivePubkey(Some(author.public_key())));
        core.install_replaceable_materializer(ReplaceableMaterializerRegistration {
            program: [9; 16],
            format: [10; 16],
            materializer: Arc::new(AddPeople),
        });
        let operation = nmp_grammar::ReplaceableOperation::from_registered_default_parts(
            [9; 16],
            [10; 16],
            Kind::ContactList,
            String::new(),
            added_person.to_bytes().to_vec(),
        )
        .expect("the first-value fallback names a valid replaceable coordinate");

        let mut preparation = core.prepare_publish(WriteIntent {
            payload: WritePayload::ReplaceableOperation(operation),
            routing: WriteRouting::Explicit(vec![destination]),
            identity: Identity::Active,
            correlation: None,
        });
        let effects = loop {
            match preparation {
                PublishPreparation::Complete(effects) => break effects,
                PublishPreparation::Materialize(prepared) => {
                    let PreparedReplaceableMaterialization { call, continuation } = *prepared;
                    let outcome = core.run_replaceable_materialization(call);
                    preparation =
                        core.complete_body_complete_replaceable_operation(continuation, outcome);
                }
            }
        };
        assert!(
            effects
                .iter()
                .all(|effect| !matches!(effect, Effect::PublishFailed(_))),
            "a source that became known before preparation must be used rather than refused: {effects:#?}"
        );
        let current = core
            .store
            .query_newest(
                &nostr::Filter::new()
                    .kind(Kind::ContactList)
                    .author(author.public_key()),
                1,
            )
            .unwrap()
            .pop()
            .expect("the materialized current row exists");
        assert_eq!(current.event.content, "known relay body");
        assert!(current
            .event
            .tags
            .iter()
            .any(|tag| tag.as_slice() == ["p", &existing_person.to_hex()]));
        assert!(current
            .event
            .tags
            .iter()
            .any(|tag| tag.as_slice() == ["p", &added_person.to_hex()]));
    }

    #[test]
    fn newer_relay_sources_install_complete_successors_without_new_receipts() {
        let author = Keys::generate();
        let alice = Keys::generate().public_key();
        let bob = Keys::generate().public_key();
        let carol = Keys::generate().public_key();
        let dave = Keys::generate().public_key();
        let relay = RelayUrl::parse("wss://semantic-source.example").unwrap();
        let destination_a = RelayUrl::parse("wss://semantic-a.example").unwrap();
        let destination_b = RelayUrl::parse("wss://semantic-b.example").unwrap();
        let base = source(&author, 1, "base", &[]);
        let mut store = RedbStore::temporary().expect("temporary Redb store");
        store
            .insert(
                base.clone(),
                RelayObserved::new(relay.clone(), Timestamp::from(1)),
            )
            .unwrap();
        let mut core = EngineCore::new(store, 10);
        core.handle(EngineMsg::SetActivePubkey(Some(author.public_key())));
        core.handle(EngineMsg::Subscribe(LiveQuery::from_filter(
            nmp_grammar::Filter {
                kinds: Some(BTreeSet::from([Kind::ContactList.as_u16()])),
                authors: Some(nmp_grammar::Binding::Literal(BTreeSet::from([author
                    .public_key()
                    .to_hex()]))),
                ..nmp_grammar::Filter::default()
            },
        )));
        core.install_replaceable_materializer(ReplaceableMaterializerRegistration {
            program: [6; 16],
            format: [7; 16],
            materializer: Arc::new(AddPeople),
        });

        let original = UnsignedEvent::from(base.clone());
        let mut current = original.clone();
        let mut receipts = Vec::new();
        let mut e1_sign_request = None;
        for (person, destination) in [(alice, destination_a.clone()), (bob, destination_b.clone())]
        {
            let payload = nmp_grammar::ReplaceableOperation::from_registered_parts(
                [6; 16],
                [7; 16],
                original.clone(),
                current.clone(),
                person.to_bytes().to_vec(),
            )
            .unwrap();
            let effects = core.handle(EngineMsg::Publish(WriteIntent {
                payload: WritePayload::ReplaceableOperation(payload),
                routing: WriteRouting::Explicit(vec![destination]),
                identity: Identity::Active,
                correlation: None,
            }));
            let (receipt, event_id) = effects
                .iter()
                .find_map(|effect| match effect {
                    Effect::WriteAccepted(receipt, event_id) => Some((*receipt, *event_id)),
                    _ => None,
                })
                .unwrap();
            receipts.push(receipt);
            e1_sign_request = effects.iter().find_map(|effect| match effect {
                Effect::RequestSign(receipt, generation, unsigned) => {
                    Some((*receipt, *generation, unsigned.clone()))
                }
                _ => None,
            });
            current = UnsignedEvent::from(
                core.store
                    .query(&nostr::Filter::new().id(event_id))
                    .unwrap()
                    .pop()
                    .unwrap()
                    .event,
            );
        }

        let first_local_id = current.id.unwrap();
        let (e1_owner, e1_generation, e1_unsigned) =
            e1_sign_request.expect("current E1 requests one signature");
        let e1_signed = e1_unsigned.sign_with_keys(&author).unwrap();
        core.handle(EngineMsg::SignerCompleted(
            e1_owner,
            e1_generation,
            Ok(e1_signed.clone()),
        ));
        for (slot, destination) in [&destination_a, &destination_b].into_iter().enumerate() {
            let session = RelaySessionKey::new(
                destination.clone(),
                AccessContext::Nip42(author.public_key()),
            );
            let handle = TransportRelayHandle {
                slot: u32::try_from(slot).unwrap(),
                generation: 1,
            };
            let read_session = RelaySessionKey::public(destination.clone());
            let read_handle = TransportRelayHandle {
                slot: u32::try_from(slot).unwrap().saturating_add(32),
                generation: 1,
            };
            core.handle(EngineMsg::RelayConnected(read_handle, read_session.clone()));
            core.handle(EngineMsg::RelayConnected(handle, session.clone()));
            let parked = core.handle(EngineMsg::AuthProbeReleased(handle, session.clone()));
            let released =
                core.answer_coordinate_coverage_for_test(&[(read_handle, read_session)], &parked);
            let correlation = released
                .iter()
                .find_map(|effect| match effect {
                    Effect::PublishEvent(candidate, event, correlation)
                        if candidate == &session && event.id == e1_signed.id =>
                    {
                        Some(*correlation)
                    }
                    _ => None,
                })
                .expect("E1 starts on every destination");
            core.handle(EngineMsg::EventHandoff(correlation, HandoffResult::Written));
            // Only the first destination acknowledges. #1631 ends active
            // semantic work the moment routing is closed and EVERY lane of
            // the current generation is terminal, so acknowledging both here
            // would settle the cohort and delete the very receipts this test
            // is about to prove a successor rides. One destination still
            // outstanding is also the exact shape of the epic's own
            // scenario: r1 published, r2 has not.
            if destination == &destination_a {
                core.handle(EngineMsg::RelayFrame(
                    handle,
                    session.clone(),
                    RelayFrame::from(nostr::RelayMessage::ok(e1_signed.id, true, "saved")),
                ));
            }
            core.handle(EngineMsg::RelayDisconnected(
                handle,
                session,
                DisconnectReason::Closed,
            ));
        }
        let stale_generation = 9;
        let stale_unsigned = current.clone();
        let stale_signed = stale_unsigned.sign_with_keys(&author).unwrap();
        let stale_receipt = receipts[0];
        {
            let pending = core.pending.get_mut(&stale_receipt).unwrap();
            pending.sign_request_in_flight = true;
            pending.sign_generation = stale_generation;
        }
        let current_echo = current.clone().sign_with_keys(&author).unwrap();
        let mut echo_effects = Vec::new();
        core.ingest_relay_events(
            vec![(
                current_echo,
                RelayObserved::new(relay.clone(), Timestamp::from(2)),
            )],
            &mut echo_effects,
        );
        assert_eq!(
            core.store
                .replaceable_operation_snapshot(&Coordinate {
                    kind: Kind::ContactList,
                    public_key: author.public_key(),
                    identifier: String::new(),
                })
                .unwrap()
                .unwrap()
                .current
                .generation
                .unwrap()
                .materialization
                .event_id,
            first_local_id,
            "a relay echo of E1 is signature evidence, not a newer semantic base"
        );
        core.replaceable_materializers.clear();
        let unavailable = source(&author, 3, "unavailable", &[carol]);
        let mut unavailable_effects = Vec::new();
        core.ingest_relay_events(
            vec![(
                unavailable,
                RelayObserved::new(relay.clone(), Timestamp::from(3)),
            )],
            &mut unavailable_effects,
        );
        assert!(unavailable_effects
            .iter()
            .any(|effect| matches!(effect, Effect::EmitDiagnostics(_))));
        let unavailable_snapshot = core
            .store
            .replaceable_operation_snapshot(&Coordinate {
                kind: Kind::ContactList,
                public_key: author.public_key(),
                identifier: String::new(),
            })
            .unwrap()
            .unwrap();
        assert_eq!(unavailable_snapshot.source.unwrap().event.id, base.id);
        assert_eq!(
            unavailable_snapshot
                .current
                .generation
                .unwrap()
                .materialization
                .event_id,
            first_local_id
        );
        assert_eq!(
            core.store
                .query(
                    &nostr::Filter::new()
                        .kind(Kind::ContactList)
                        .author(author.public_key())
                )
                .unwrap()[0]
                .event
                .id,
            first_local_id
        );
        core.install_replaceable_materializer(ReplaceableMaterializerRegistration {
            program: [6; 16],
            format: [7; 16],
            materializer: Arc::new(AddPeople),
        });

        let newer = source(&author, 5, "remote-five", &[carol]);
        let mut effects = Vec::new();
        core.ingest_relay_events(
            vec![(
                newer.clone(),
                RelayObserved::new(relay.clone(), Timestamp::from(5)),
            )],
            &mut effects,
        );

        let first_successor = core
            .store
            .query(
                &nostr::Filter::new()
                    .kind(Kind::ContactList)
                    .author(author.public_key()),
            )
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(first_successor.event.created_at, Timestamp::from(6));
        assert_eq!(first_successor.event.content, "remote-five");
        for person in [alice, bob, carol] {
            assert!(first_successor
                .event
                .tags
                .iter()
                .any(|tag| tag.as_slice() == ["p", &person.to_hex()]));
        }
        assert_ne!(first_successor.event.id, first_local_id);
        assert!(effects.iter().any(|effect| match effect {
            Effect::EmitRows(_, deltas, _) => {
                deltas.len() == 2
                    && deltas.iter().any(
                        |delta| matches!(delta, RowDelta::Removed(id) if *id == first_local_id),
                    )
                    && deltas.iter().any(|delta| {
                        matches!(delta, RowDelta::Added(row) if row.id() == first_successor.event.id)
                    })
                    && deltas.iter().all(|delta| {
                        !matches!(delta, RowDelta::Added(row) if row.id() == newer.id)
                    })
            }
            _ => false,
        }), "successor effects: {effects:#?}");
        let stale_effects = core.handle(EngineMsg::SignerCompleted(
            stale_receipt,
            stale_generation,
            Ok(stale_signed),
        ));
        assert!(stale_effects.is_empty());
        assert_eq!(
            core.store
                .query(
                    &nostr::Filter::new()
                        .kind(Kind::ContactList)
                        .author(author.public_key())
                )
                .unwrap()[0]
                .event
                .id,
            first_successor.event.id,
            "a delayed E1 signature cannot promote or replace E2"
        );
        let (first_successor_owner, first_successor_generation, first_successor_unsigned) = effects
            .iter()
            .find_map(|effect| match effect {
                Effect::RequestSign(receipt, generation, unsigned)
                    if unsigned.id == Some(first_successor.event.id) =>
                {
                    Some((*receipt, *generation, unsigned.clone()))
                }
                _ => None,
            })
            .expect("relay-source successor requests one signature");
        let first_successor_signed = first_successor_unsigned.sign_with_keys(&author).unwrap();
        let first_successor_signed_effects = core.handle(EngineMsg::SignerCompleted(
            first_successor_owner,
            first_successor_generation,
            Ok(first_successor_signed),
        ));
        let expected_first_successor_sessions = BTreeSet::from([
            RelaySessionKey::new(
                destination_a.clone(),
                AccessContext::Nip42(author.public_key()),
            ),
            RelaySessionKey::new(
                destination_b.clone(),
                AccessContext::Nip42(author.public_key()),
            ),
        ]);
        assert_eq!(
            first_successor_signed_effects
                .iter()
                .filter_map(|effect| match effect {
                    Effect::EnsureWriteRelay(session) => Some(session.clone()),
                    _ => None,
                })
                .collect::<BTreeSet<_>>(),
            expected_first_successor_sessions,
            "#1470: signing the relay-source successor must reacquire every persisted E2 lane"
        );
        assert_eq!(
            core.relay_worker_requirements().unwrap().writes,
            expected_first_successor_sessions,
            "#1470: predecessor retirement must not erase current E2 worker ownership"
        );

        let erin = Keys::generate().public_key();
        let later_operation = nmp_grammar::ReplaceableOperation::from_registered_parts(
            [6; 16],
            [7; 16],
            original,
            UnsignedEvent::from(first_successor.event.clone()),
            erin.to_bytes().to_vec(),
        )
        .unwrap();
        let accepted_later = core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::ReplaceableOperation(later_operation),
            routing: WriteRouting::Explicit(vec![destination_a.clone()]),
            identity: Identity::Active,
            correlation: None,
        }));
        receipts.push(
            accepted_later
                .iter()
                .find_map(|effect| match effect {
                    Effect::WriteAccepted(receipt, _) => Some(*receipt),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("operation after B5 was refused: {accepted_later:#?}")),
        );
        assert_eq!(
            core.store
                .replaceable_operation_snapshot(&Coordinate {
                    kind: Kind::ContactList,
                    public_key: author.public_key(),
                    identifier: String::new(),
                })
                .unwrap()
                .unwrap()
                .source
                .unwrap()
                .event
                .id,
            newer.id,
            "accepting another operation must not regress retained B5 to the payload's old B0"
        );

        let later = source(&author, 7, "remote-seven", &[carol, dave]);
        let mut later_effects = Vec::new();
        core.ingest_relay_events(
            vec![(later, RelayObserved::new(relay, Timestamp::from(7)))],
            &mut later_effects,
        );
        let second_successor = core
            .store
            .query(
                &nostr::Filter::new()
                    .kind(Kind::ContactList)
                    .author(author.public_key()),
            )
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(second_successor.event.created_at, Timestamp::from(8));
        assert_eq!(second_successor.event.content, "remote-seven");
        for person in [alice, bob, carol, dave, erin] {
            assert!(second_successor
                .event
                .tags
                .iter()
                .any(|tag| tag.as_slice() == ["p", &person.to_hex()]));
        }
        assert_eq!(core.pending.len(), 3);
        assert_eq!(core.intent_receipts.len(), 3);
        assert_eq!(
            receipts.iter().copied().collect::<BTreeSet<_>>(),
            core.pending.keys().copied().collect()
        );
        assert!(core
            .pending
            .values()
            .all(|pending| pending.frozen.id == second_successor.event.id));
        let sign_requests = later_effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::RequestSign(receipt, generation, unsigned) => {
                    Some((*receipt, *generation, unsigned.clone()))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            sign_requests.len(),
            1,
            "one semantic generation has one physical signer request"
        );
        let (owner, generation, unsigned) = sign_requests.into_iter().next().unwrap();
        let signed = unsigned.sign_with_keys(&author).unwrap();
        let signed_effects = core.handle(EngineMsg::SignerCompleted(
            owner,
            generation,
            Ok(signed.clone()),
        ));
        for receipt in &receipts {
            assert!(signed_effects.iter().any(|effect| matches!(
                effect,
                Effect::EmitReceipt(
                    candidate,
                    WriteFact::Signing(SigningState::Signed { event_id })
                ) if candidate == receipt && *event_id == signed.id
            )));
        }
        let owner_intent = core.pending[&owner].intent_id;
        let lanes = core
            .store
            .recover_publish_queue_lanes(owner_intent)
            .unwrap();
        assert_eq!(
            lanes
                .iter()
                .map(|lane| lane.key.relay.clone())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([destination_a, destination_b]),
            "all contributing routes union into the one physical owner"
        );
        assert!(core.pending.iter().all(|(receipt, pending)| {
            *receipt == owner
                || core
                    .store
                    .recover_publish_queue_lanes(pending.intent_id)
                    .unwrap()
                    .is_empty()
        }));
    }

    /// #1606: a replaceable-operation successor that rewrites an existing
    /// member's `pending` row in place must not leave that member's OLD
    /// generation naming it in `receipts_by_lane_relay`.
    ///
    /// Both successor-rewrite sites (`write.rs`'s
    /// `install_materialized_replaceable_successor` and
    /// `write/replaceable_operation.rs`'s member-rewrite branch) used to
    /// assign `LaneWorkerProjection::default()` to `pending.lane_projection`
    /// directly and never told this index. The owner's receipt would then
    /// keep naming the relay its now-superseded generation had a persisted
    /// lane on until the next full boot recovery -- a permanent phantom
    /// wake candidate for a generation that no longer exists.
    #[test]
    fn a_successor_rewrite_releases_the_owners_old_lane_relay_index_entry() {
        let author = Keys::generate();
        let alice = Keys::generate().public_key();
        let bob = Keys::generate().public_key();
        let relay = RelayUrl::parse("wss://successor-leak-source.example").unwrap();
        let destination_a = RelayUrl::parse("wss://successor-leak-a.example").unwrap();
        let destination_b = RelayUrl::parse("wss://successor-leak-b.example").unwrap();
        let base = source(&author, 1, "base", &[]);
        let mut store = RedbStore::temporary().expect("temporary Redb store");
        store
            .insert(
                base.clone(),
                RelayObserved::new(relay.clone(), Timestamp::from(1)),
            )
            .unwrap();
        let mut core = EngineCore::new(store, 10);
        core.handle(EngineMsg::SetActivePubkey(Some(author.public_key())));
        core.handle(EngineMsg::Subscribe(LiveQuery::from_filter(
            nmp_grammar::Filter {
                kinds: Some(BTreeSet::from([Kind::ContactList.as_u16()])),
                authors: Some(nmp_grammar::Binding::Literal(BTreeSet::from([author
                    .public_key()
                    .to_hex()]))),
                ..nmp_grammar::Filter::default()
            },
        )));
        core.install_replaceable_materializer(ReplaceableMaterializerRegistration {
            program: [9; 16],
            format: [9; 16],
            materializer: Arc::new(AddPeople),
        });

        let original = UnsignedEvent::from(base.clone());
        let mut current = original.clone();
        let mut receipts = Vec::new();
        let mut e1_sign_request = None;
        for (person, destination) in [(alice, destination_a.clone()), (bob, destination_b.clone())]
        {
            let payload = nmp_grammar::ReplaceableOperation::from_registered_parts(
                [9; 16],
                [9; 16],
                original.clone(),
                current.clone(),
                person.to_bytes().to_vec(),
            )
            .unwrap();
            let effects = core.handle(EngineMsg::Publish(WriteIntent {
                payload: WritePayload::ReplaceableOperation(payload),
                routing: WriteRouting::Explicit(vec![destination]),
                identity: Identity::Active,
                correlation: None,
            }));
            let (receipt, event_id) = effects
                .iter()
                .find_map(|effect| match effect {
                    Effect::WriteAccepted(receipt, event_id) => Some((*receipt, *event_id)),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("contribution was refused: {effects:#?}"));
            receipts.push(receipt);
            e1_sign_request = effects.iter().find_map(|effect| match effect {
                Effect::RequestSign(receipt, generation, unsigned) => {
                    Some((*receipt, *generation, unsigned.clone()))
                }
                _ => None,
            });
            current = UnsignedEvent::from(
                core.store
                    .query(&nostr::Filter::new().id(event_id))
                    .unwrap()
                    .pop()
                    .unwrap()
                    .event,
            );
        }
        // #1606, the second index: publishing bob's contribution rewrote
        // alice's already-pending row onto the new generation through
        // `write/replaceable_operation.rs`'s member-rewrite branch. That
        // branch retires the member's old frozen bytes, and it used to drop
        // the receipt from `event_to_receipts` WITHOUT pruning the emptied
        // set -- while the sibling rewrite site in `write.rs` pruned. One
        // rewrite, two spellings, and the divergent one left an entry
        // asserting that bytes no receipt owns are still owned, once per
        // rewrite, until the next boot recovery.
        assert!(
            !core.event_to_receipts.is_empty(),
            "precondition: the scenario must index some frozen bytes, or the emptiness \
             assertion below is vacuous"
        );
        assert!(
            core.event_to_receipts
                .values()
                .all(|receipts| !receipts.is_empty()),
            "a retired generation's event id survives in event_to_receipts owned by no \
             receipt -- the member rewrite released the receipt without pruning the \
             entry: {:?}",
            core.event_to_receipts
        );

        // Only the generation's first member -- the physical delivery
        // owner -- ever carries routes and lanes; the request for E1's one
        // signature names exactly who that is.
        let owner_receipt = receipts[0];
        let (e1_owner, e1_generation, e1_unsigned) =
            e1_sign_request.expect("current E1 requests one signature");
        assert_eq!(
            e1_owner, owner_receipt,
            "the first contributor is expected to own E1's signature and lanes"
        );
        let e1_signed = e1_unsigned.sign_with_keys(&author).unwrap();
        core.handle(EngineMsg::SignerCompleted(
            e1_owner,
            e1_generation,
            Ok(e1_signed),
        ));

        // Precondition, asserted before the fact that matters: signing E1
        // resolved its explicit route and bootstrapped its lane, which is
        // what populates `receipts_by_lane_relay` -- independent of any
        // relay connection. Without this the test below could pass because
        // the scenario never indexed the receipt in the first place, not
        // because the rewrite released it.
        assert!(
            core.receipts_by_lane_relay
                .get(&destination_a)
                .is_some_and(|receipts| receipts.contains(&owner_receipt)),
            "setup did not persist E1's lane for the owner's receipt on {destination_a}: {:?}",
            core.receipts_by_lane_relay
        );

        // A newer relay-observed source triggers a successor materialization
        // that rewrites the owner's `pending` row onto a new generation --
        // through `install_materialized_replaceable_successor`, since the
        // owner already exists in `pending`/`intent_receipts`.
        let newer = source(&author, 5, "successor-leak-newer", &[]);
        let mut effects = Vec::new();
        core.ingest_relay_events(
            vec![(
                newer.clone(),
                RelayObserved::new(relay.clone(), Timestamp::from(5)),
            )],
            &mut effects,
        );
        // The postcondition that matters: the owner's OLD generation is
        // superseded and its lane_projection was reset, so its relay must
        // not still be naming this receipt.
        assert!(
            !core
                .receipts_by_lane_relay
                .get(&destination_a)
                .is_some_and(|receipts| receipts.contains(&owner_receipt)),
            "the owner's receipt is still indexed under {destination_a} after its successor \
             rewrite -- receipts_by_lane_relay leaked the superseded generation's lane: {:?}",
            core.receipts_by_lane_relay
        );
    }

    #[test]
    fn stale_predecessor_delivery_callbacks_cannot_touch_the_current_successor() {
        let author = Keys::generate();
        let person = Keys::generate().public_key();
        let source_relay = RelayUrl::parse("wss://semantic-source.example").unwrap();
        let handoff_relay = RelayUrl::parse("wss://stale-handoff.example").unwrap();
        let ack_relay = RelayUrl::parse("wss://stale-ack.example").unwrap();
        let auth_relay = RelayUrl::parse("wss://stale-auth.example").unwrap();
        let timeout_relay = RelayUrl::parse("wss://stale-timeout.example").unwrap();
        let retry_relay = RelayUrl::parse("wss://stale-retry.example").unwrap();
        let destinations = [
            handoff_relay.clone(),
            ack_relay.clone(),
            auth_relay.clone(),
            timeout_relay.clone(),
            retry_relay.clone(),
        ];
        let base = source(&author, 1, "base", &[]);
        let mut store = RedbStore::temporary().expect("temporary Redb store");
        store
            .insert(
                base.clone(),
                RelayObserved::new(source_relay.clone(), Timestamp::from(1)),
            )
            .unwrap();
        let mut core = EngineCore::new(store, 10);
        core.handle(EngineMsg::SetActivePubkey(Some(author.public_key())));
        core.install_replaceable_materializer(ReplaceableMaterializerRegistration {
            program: [16; 16],
            format: [17; 16],
            materializer: Arc::new(AddPeople),
        });

        let original = UnsignedEvent::from(base);
        let operation = nmp_grammar::ReplaceableOperation::from_registered_parts(
            [16; 16],
            [17; 16],
            original.clone(),
            original,
            person.to_bytes().to_vec(),
        )
        .unwrap();
        let accepted = core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::ReplaceableOperation(operation),
            routing: WriteRouting::Explicit(destinations.to_vec()),
            identity: Identity::Active,
            correlation: None,
        }));
        let (receipt, e1_generation, e1_unsigned) = accepted
            .iter()
            .find_map(|effect| match effect {
                Effect::RequestSign(receipt, generation, unsigned) => {
                    Some((*receipt, *generation, unsigned.clone()))
                }
                _ => None,
            })
            .expect("E1 requests one signature");
        let e1 = e1_unsigned.sign_with_keys(&author).unwrap();
        core.handle(EngineMsg::SignerCompleted(
            receipt,
            e1_generation,
            Ok(e1.clone()),
        ));

        let sessions = destinations
            .iter()
            .cloned()
            .map(|relay| RelaySessionKey::new(relay, AccessContext::Nip42(author.public_key())))
            .collect::<Vec<_>>();
        let handles = sessions
            .iter()
            .enumerate()
            .map(|(slot, _)| TransportRelayHandle {
                slot: u32::try_from(slot).unwrap(),
                generation: 1,
            })
            .collect::<Vec<_>>();
        let mut e1_correlations = BTreeMap::new();
        for ((handle, session), relay) in handles.iter().zip(&sessions).zip(&destinations) {
            let read_session = RelaySessionKey::public(relay.clone());
            let read_handle = TransportRelayHandle {
                slot: handle.slot.saturating_add(32),
                generation: 1,
            };
            core.handle(EngineMsg::RelayConnected(read_handle, read_session.clone()));
            core.handle(EngineMsg::RelayConnected(*handle, session.clone()));
            let parked = core.handle(EngineMsg::AuthProbeReleased(*handle, session.clone()));
            let released =
                core.answer_coordinate_coverage_for_test(&[(read_handle, read_session)], &parked);
            let correlation = released
                .iter()
                .find_map(|effect| match effect {
                    Effect::PublishEvent(candidate, event, correlation)
                        if candidate == session && event.id == e1.id =>
                    {
                        Some(*correlation)
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("E1 did not start on {relay}"));
            e1_correlations.insert(relay.clone(), correlation);
        }

        for relay in [&ack_relay, &auth_relay, &timeout_relay, &retry_relay] {
            core.handle(EngineMsg::EventHandoff(
                e1_correlations[relay],
                HandoffResult::Written,
            ));
        }
        core.handle(EngineMsg::RelayFrame(
            handles[4],
            sessions[4].clone(),
            RelayFrame::from(nostr::RelayMessage::ok(
                e1.id,
                false,
                "rate-limited: retry later",
            )),
        ));

        let e1_intent = core.pending[&receipt].intent_id;
        let e1_lanes = core.store.recover_publish_queue_lanes(e1_intent).unwrap();
        let state_for = |relay: &RelayUrl| {
            e1_lanes
                .iter()
                .find(|lane| &lane.key.relay == relay)
                .unwrap_or_else(|| panic!("missing E1 lane for {relay}"))
                .state
                .clone()
        };
        assert!(matches!(
            state_for(&handoff_relay),
            PublishQueueLaneState::InFlight {
                phase: PublishQueueInFlightPhase::AwaitingHandoff,
                ..
            }
        ));
        for relay in [&ack_relay, &auth_relay, &timeout_relay] {
            assert!(matches!(
                state_for(relay),
                PublishQueueLaneState::InFlight {
                    phase: PublishQueueInFlightPhase::AwaitingAck { .. },
                    ..
                }
            ));
        }
        let retry_due = match state_for(&retry_relay) {
            PublishQueueLaneState::Transient { eligible_at, .. } => eligible_at,
            state => panic!("E1 retry lane is not transient: {state:?}"),
        };
        let timeout_due = match state_for(&timeout_relay) {
            PublishQueueLaneState::InFlight {
                phase: PublishQueueInFlightPhase::AwaitingAck { deadline },
                ..
            } => deadline,
            state => panic!("E1 timeout lane is not awaiting an ACK: {state:?}"),
        };

        let newer = source(&author, 5, "newer source", &[]);
        let mut installed = Vec::new();
        core.ingest_relay_events(
            vec![(newer, RelayObserved::new(source_relay, Timestamp::from(5)))],
            &mut installed,
        );
        let (e2_generation, e2_unsigned) = installed
            .iter()
            .find_map(|effect| match effect {
                Effect::RequestSign(candidate, generation, unsigned) if *candidate == receipt => {
                    Some((*generation, unsigned.clone()))
                }
                _ => None,
            })
            .expect("E2 requests one signature");
        let e2 = e2_unsigned.sign_with_keys(&author).unwrap();
        assert_ne!(e2.id, e1.id);
        // Keep E2's own ACK deadline strictly after both retired E1
        // deadlines. The later ticks below are therefore stale E1 inputs,
        // not legitimate current-generation timeouts.
        core.clock = Timestamp::from(10_000u64);
        let e2_started = core.handle(EngineMsg::SignerCompleted(
            receipt,
            e2_generation,
            Ok(e2.clone()),
        ));
        // E2 is a new generation, so every lane asks the per-relay
        // coordinate question again before it may take an attempt (#1631).
        let read_sessions = handles
            .iter()
            .zip(&destinations)
            .map(|(handle, relay)| {
                (
                    TransportRelayHandle {
                        slot: handle.slot.saturating_add(32),
                        generation: 1,
                    },
                    RelaySessionKey::public(relay.clone()),
                )
            })
            .collect::<Vec<_>>();
        let e2_answered = core.answer_coordinate_coverage_for_test(&read_sessions, &e2_started);
        let mut combined = e2_started;
        combined.extend(e2_answered);
        let e2_started = combined;
        assert_eq!(
            e2_started
                .iter()
                .filter(|effect| matches!(
                    effect,
                    Effect::PublishEvent(_, event, _) if event.id == e2.id
                ))
                .count(),
            destinations.len(),
            "E2 must own one fresh attempt at every current destination"
        );
        for correlation in e2_started.iter().filter_map(|effect| match effect {
            Effect::PublishEvent(_, event, correlation) if event.id == e2.id => Some(*correlation),
            _ => None,
        }) {
            core.handle(EngineMsg::EventHandoff(correlation, HandoffResult::Written));
        }

        let e2_lanes_before = core.store.recover_publish_queue_lanes(e1_intent).unwrap();
        assert_eq!(e2_lanes_before.len(), destinations.len());
        assert!(e2_lanes_before.iter().all(|lane| {
            lane.key.event_id == e2.id
                && matches!(
                    lane.state,
                    PublishQueueLaneState::InFlight {
                        phase: PublishQueueInFlightPhase::AwaitingAck { .. },
                        ..
                    }
                )
        }));
        let semantic_before = core
            .store
            .replaceable_operation_snapshot(&Coordinate {
                kind: Kind::ContactList,
                public_key: author.public_key(),
                identifier: String::new(),
            })
            .unwrap();
        let receipt_before = core.reattach_receipt(receipt).facts;
        let e2_generation_before = core.pending[&receipt].sign_generation;

        let stale_batches = [
            core.handle(EngineMsg::EventHandoff(
                e1_correlations[&handoff_relay],
                HandoffResult::Written,
            )),
            core.handle(EngineMsg::RelayFrame(
                handles[1],
                sessions[1].clone(),
                RelayFrame::from(nostr::RelayMessage::ok(e1.id, true, "saved")),
            )),
            core.handle(EngineMsg::RelayFrame(
                handles[2],
                sessions[2].clone(),
                RelayFrame::from(nostr::RelayMessage::ok(
                    e1.id,
                    false,
                    "auth-required: authenticate",
                )),
            )),
            core.handle(EngineMsg::Tick(retry_due)),
            core.handle(EngineMsg::Tick(timeout_due)),
        ];
        for effects in &stale_batches {
            assert!(
                effects.iter().all(|effect| !matches!(
                    effect,
                    Effect::PublishEvent(_, event, _) if event.id == e1.id
                )),
                "a stale predecessor callback put E1 back on the wire: {effects:#?}"
            );
            assert!(
                effects.iter().all(|effect| !matches!(
                    effect,
                    Effect::EmitReceipt(_, WriteFact::Relay { event_id, .. })
                        if *event_id == e1.id || *event_id == e2.id
                )),
                "a stale predecessor callback advanced a current or retired relay fact: {effects:#?}"
            );
        }

        let e2_lanes_after = core.store.recover_publish_queue_lanes(e1_intent).unwrap();
        assert_eq!(
            e2_lanes_after, e2_lanes_before,
            "stale E1 handoff, ACK, AUTH-required result, timeout, or retry changed E2"
        );
        assert_eq!(
            core.store
                .replaceable_operation_snapshot(&Coordinate {
                    kind: Kind::ContactList,
                    public_key: author.public_key(),
                    identifier: String::new(),
                })
                .unwrap(),
            semantic_before,
            "stale delivery callbacks changed the semantic generation"
        );
        assert_eq!(core.reattach_receipt(receipt).facts, receipt_before);
        assert_eq!(core.pending[&receipt].frozen.id, e2.id);
        assert_eq!(
            core.pending[&receipt].sign_generation, e2_generation_before,
            "stale delivery callbacks advanced the current signer generation"
        );
        assert!(
            !core.auth_required_sessions.contains(&sessions[2]),
            "an auth-required result for retired E1 must not park E2"
        );
        assert!(
            !core.retry_scheduler_blocked,
            "retired E1 deadlines must be gone rather than poisoning the current scheduler"
        );
    }
}

/// The receipt-replay cursor must keep the two persistence stalls apart.
///
/// One relay can stall on BOTH its append-only route revision and its attempt
/// log. The app-facing shape is deliberately one `PersistenceStalled { detail }`
/// (#1237: the difference is a recovery detail, not an app decision), but the
/// replay cursor still has to dedup them SEPARATELY — keying on the relay
/// alone silently swallows whichever arrives second, and a durable receipt
/// that loses a fact under paging is the exact class of loss it exists to
/// prevent.
#[cfg(test)]
mod persistence_stall_replay_tests {
    use super::*;
    use nmp_router_testkit::FixtureRoutingFacts;
    use nmp_store::RedbStore;
    use nostr::{Keys, RelayUrl};

    #[test]
    fn one_relay_stalled_on_both_route_and_attempt_replays_both_facts() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://stalled.example").unwrap();
        let mut core = EngineCore::new_with_fixture_routing_facts(
            RedbStore::temporary().expect("temporary Redb store"),
            FixtureRoutingFacts::new(),
            4,
        );
        core.handle(EngineMsg::SetActivePubkey(Some(keys.public_key())));

        let accepted = core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::Event(nmp_grammar::EventBuilder {
                kind: nostr::Kind::TextNote,
                tags: Vec::new(),
                content: "stalled on both".to_string(),
                created_at: Some(Timestamp::from(1_000u64)),
            }),
            routing: WriteRouting::Explicit(vec![relay.clone()]),
            identity: Identity::Active,
            correlation: None,
        }));
        let id = accepted
            .iter()
            .find_map(|effect| match effect {
                Effect::WriteAccepted(id, _) => Some(*id),
                _ => None,
            })
            .expect("publish takes custody and answers with the receipt id");
        // The same relay failed to commit BOTH durable facts.
        let pending = core.pending.get_mut(&id).expect("the write is pending");
        pending.unstarted_relays.insert(relay.clone());
        pending.route_blocked_relays.insert(relay.clone());

        // Page one fact at a time, which is what makes a collapsed dedup key
        // observable: the second stall is skipped as "already delivered".
        let mut cursor = None;
        let mut stalls = Vec::new();
        for _ in 0..8 {
            let page = core.reattach_receipt_page(id, cursor.clone(), 1);
            if page.facts.is_empty() {
                break;
            }
            for fact in &page.facts {
                if let WriteFact::Relay {
                    state: RelayState::Waiting(RelayWaiting::PersistenceStalled { detail }),
                    ..
                } = fact
                {
                    stalls.push(detail.clone());
                }
            }
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }

        assert!(
            stalls.iter().any(|d| d == ATTEMPT_STALL_DETAIL),
            "the attempt-log stall was lost under paged reattachment: {stalls:?}"
        );
        assert!(
            stalls.iter().any(|d| d == ROUTE_STALL_DETAIL),
            "the route-revision stall was lost under paged reattachment; keying the replay \
             cursor on the relay alone swallows whichever stall arrives second: {stalls:?}"
        );
    }

    /// The latch mosaico's `persistence_blockage_remains_visible_after_later_ack`
    /// specifies: a fault observed once stays readable on the entry even after
    /// a relay succeeds afterwards. An operator must not lose the only signal
    /// that the local disk is failing because something later went right.
    #[test]
    fn a_persistence_fault_survives_a_later_success_on_the_same_write() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://latched.example").unwrap();
        let mut core = EngineCore::new_with_fixture_routing_facts(
            RedbStore::temporary().expect("temporary Redb store"),
            FixtureRoutingFacts::new(),
            4,
        );
        core.handle(EngineMsg::SetActivePubkey(Some(keys.public_key())));
        let accepted = core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::Event(nmp_grammar::EventBuilder {
                kind: nostr::Kind::TextNote,
                tags: Vec::new(),
                content: "latched".to_string(),
                created_at: Some(Timestamp::from(2_000u64)),
            }),
            routing: WriteRouting::Explicit(vec![relay.clone()]),
            identity: Identity::Active,
            correlation: None,
        }));
        let id = accepted
            .iter()
            .find_map(|effect| match effect {
                Effect::WriteAccepted(id, _) => Some(*id),
                _ => None,
            })
            .expect("publish takes custody and answers with the receipt id");
        let event_id = core.pending[&id].frozen.id;

        let mut effects = Vec::new();
        core.emit_write_fact(
            id,
            WriteFact::Relay {
                event_id,
                relay: relay.clone(),
                state: RelayState::Waiting(RelayWaiting::PersistenceStalled {
                    detail: ATTEMPT_STALL_DETAIL.to_string(),
                }),
            },
            &mut effects,
        );
        assert_eq!(
            core.pending
                .get(&id)
                .and_then(|pending| pending.persistence_fault.clone())
                .as_deref(),
            Some(ATTEMPT_STALL_DETAIL),
            "the fault must be readable on the entry, not only observable in the stream"
        );

        // A later success at the same relay.
        core.emit_write_fact(
            id,
            WriteFact::Relay {
                event_id,
                relay,
                state: RelayState::Published,
            },
            &mut effects,
        );
        assert_eq!(
            core.pending
                .get(&id)
                .and_then(|pending| pending.persistence_fault.clone())
                .as_deref(),
            Some(ATTEMPT_STALL_DETAIL),
            "a later ack overwrote the persistence fault; an operator loses the only signal \
             that the local disk is failing"
        );
    }
}
