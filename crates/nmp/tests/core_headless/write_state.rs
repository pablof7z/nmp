use super::*;

// ---- write projection and lifecycle ------------------------------------

#[test]
fn durable_pending_row_is_visible_before_signer_and_tamper_compensates() {
    let a = Keys::generate();
    let relay = RelayUrl::parse("wss://write.example.com").unwrap();
    let dir = FixtureRoutingFacts::new().with_outbound_routes(a.public_key(), [relay]);
    let mut core = new_core(dir);
    activate(&mut core, &a);
    core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &a.public_key().to_hex(),
    )));

    let effects = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(draft(10, "accepted body")),
        routing: WriteRouting::Auto,
        identity: Identity::Active,
        correlation: None,
    }));
    let (id, generation, accepted_template) = find_sign_request(&effects);
    let accepted_id = accepted_template.clone().sign_with_keys(&a).unwrap().id;
    assert!(all_row_deltas(&effects).iter().any(|delta| matches!(
        delta,
        RowDelta::Added(row)
            if row.id() == accepted_id
                && row.signature() == nmp::RowSignature::Pending
    )));
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::WriteAccepted(rid, _) if *rid == id)),
        "publish takes custody of the write"
    );
    assert!(
        receipt_statuses(&effects).is_empty(),
        "acceptance is the Ok return, never a fact on the stream"
    );

    let tampered = signed_draft(&draft(10, "different signer output"), &a);
    let effects = core.handle(EngineMsg::SignerCompleted(id, generation, Ok(tampered)));
    assert!(
        all_row_deltas(&effects)
            .iter()
            .all(|delta| !matches!(delta, RowDelta::Updated(_))),
        "an invalid signer result must never promote the optimistic row to signed"
    );
    assert!(!effects
        .iter()
        .any(|effect| matches!(effect, Effect::PublishEvent(..))));
    assert!(all_row_deltas(&effects)
        .iter()
        .any(|delta| matches!(delta, RowDelta::Removed(event_id) if *event_id == accepted_id)));
    let facts = receipt_statuses(&effects);
    assert!(facts
        .iter()
        .any(|fact| matches!(fact, WriteFact::Signing(SigningState::Refused { .. }))));
    assert_eq!(
        facts.last(),
        Some(&WriteFact::Outcome(WriteOutcome::NotSent(
            NotSentReason::SignerRefused
        )))
    );
}

#[test]
fn cancellation_never_restores_an_unpublished_replaceable_predecessor() {
    let a = Keys::generate();
    let relay = RelayUrl::parse("wss://write.example.com").unwrap();
    let dir = FixtureRoutingFacts::new().with_outbound_routes(a.public_key(), [relay]);
    let mut core = new_core(dir);
    activate(&mut core, &a);
    core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[0],
        &a.public_key().to_hex(),
    )));

    let older_unsigned = UnsignedEvent::new(
        a.public_key(),
        Timestamp::from(1),
        Kind::Metadata,
        Vec::new(),
        "older",
    );
    let older = older_unsigned.sign_with_keys(&a).unwrap();
    core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Signed(older.clone()),
        routing: WriteRouting::Auto,
        identity: Identity::Active,
        correlation: None,
    }));

    let newer_unsigned = nmp_grammar::EventBuilder::new(Kind::Metadata)
        .content("newer")
        .created_at(Timestamp::from(2));
    let newer_id = signed_draft(&newer_unsigned, &a).id;
    let effects = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(newer_unsigned.clone()),
        routing: WriteRouting::Auto,
        identity: Identity::Active,
        correlation: None,
    }));
    let (newer_receipt, _, _) = find_sign_request(&effects);
    assert!(all_row_deltas(&effects)
        .iter()
        .any(|delta| matches!(delta, RowDelta::Added(row) if row.id() == newer_id)));
    assert!(all_row_deltas(&effects)
        .iter()
        .any(|delta| matches!(delta, RowDelta::Removed(id) if *id == older.id)));

    let (outcome, effects) = core.cancel_write(newer_receipt);
    assert_eq!(
        outcome,
        Ok(nmp::mechanism::publish_queue::CancelWriteOutcome::Cancelled)
    );
    assert_eq!(
        receipt_statuses(&effects).last(),
        Some(&WriteFact::Outcome(WriteOutcome::NotSent(
            NotSentReason::Cancelled
        )))
    );
    assert!(all_row_deltas(&effects)
        .iter()
        .any(|delta| matches!(delta, RowDelta::Removed(id) if *id == newer_id)));
    assert!(!all_row_deltas(&effects)
        .iter()
        .any(|delta| matches!(delta, RowDelta::Added(row) if row.id() == older.id)));
    let fresh = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[0],
        &a.public_key().to_hex(),
    )));
    assert!(!all_row_deltas(&fresh)
        .iter()
        .any(|delta| matches!(delta, RowDelta::Added(row) if row.id() == older.id)));
}

#[test]
fn cancellation_outcomes_are_typed_idempotent_and_late_signers_are_inert() {
    let a = Keys::generate();
    let relay = RelayUrl::parse("wss://write.example.com").unwrap();
    let mut core =
        new_core(FixtureRoutingFacts::new().with_outbound_routes(a.public_key(), [relay.clone()]));
    activate(&mut core, &a);

    let published = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(draft(10, "cancel typed")),
        routing: WriteRouting::Auto,
        identity: Identity::Active,
        correlation: None,
    }));
    let (receipt, generation, template) = find_sign_request(&published);
    let signed = template.sign_with_keys(&a).unwrap();

    let (first_outcome, first_cancelled) = core.cancel_write(receipt);
    assert_eq!(
        first_outcome,
        Ok(nmp::mechanism::publish_queue::CancelWriteOutcome::Cancelled)
    );
    assert_eq!(
        core.cancel_write(receipt).0,
        Ok(nmp::mechanism::publish_queue::CancelWriteOutcome::Cancelled)
    );
    assert!(core
        .handle(EngineMsg::SignerCompleted(receipt, generation, Ok(signed)))
        .is_empty());
    let mut statuses = receipt_statuses(&published);
    statuses.extend(receipt_statuses(&first_cancelled));
    assert_eq!(
        statuses,
        [WriteFact::Outcome(WriteOutcome::NotSent(
            NotSentReason::Cancelled
        ))]
    );
    assert!(matches!(
        core.cancel_write(ReceiptId(u64::MAX)).0,
        Err(nmp::mechanism::publish_queue::CancelWriteError::UnknownReceipt { .. })
    ));

    let signed_event = signed_draft(&draft(11, "already signed"), &a);
    let signed_publish = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Signed(signed_event.clone()),
        routing: WriteRouting::Auto,
        identity: Identity::Active,
        correlation: None,
    }));
    let signed_receipt = signed_publish
        .iter()
        .find_map(|effect| match effect {
            Effect::WriteAccepted(id, _) => Some(*id),
            _ => None,
        })
        .unwrap();
    assert!(matches!(
        core.cancel_write(signed_receipt).0,
        Err(nmp::mechanism::publish_queue::CancelWriteError::AlreadySigned {
            event_id: id,
            ..
        }) if id == signed_event.id
    ));
}

#[test]
fn signer_unavailable_keeps_accepted_row_visible() {
    let a = Keys::generate();
    let wrong = Keys::generate();
    let mut core = new_core(FixtureRoutingFacts::new());
    activate(&mut core, &a);
    let opened = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &a.public_key().to_hex(),
    )));
    let first_handle = subscribed_handle(&opened);
    let effects = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(draft(1, "awaiting signer")),
        routing: WriteRouting::Auto,
        identity: Identity::Active,
        correlation: None,
    }));
    let (id, generation, template) = find_sign_request(&effects);
    let expected_id = template.clone().sign_with_keys(&a).unwrap().id;
    let effects = core.handle(EngineMsg::SignerUnavailable(id, generation));
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::EmitReceipt(rid, WriteFact::Signing(SigningState::AwaitingSigner { pubkey }))
            if *rid == id && *pubkey == a.public_key()
    )));
    let wrong_attach = core.handle(EngineMsg::SignerAttached(wrong.public_key()));
    assert!(
        wrong_attach.is_empty(),
        "attaching a different key must neither rearm nor mutate this write"
    );

    let fresh = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &a.public_key().to_hex(),
    )));
    let second_handle = subscribed_handle(&fresh);
    assert!(all_row_deltas(&fresh).iter().any(|delta| matches!(
        delta,
        RowDelta::Added(row)
            if row.id() == expected_id
                && row.signature() == nmp::RowSignature::Pending
    )));

    let exact_attach = core.handle(EngineMsg::SignerAttached(a.public_key()));
    let (rearmed_id, rearmed_generation, rearmed_template) = find_sign_request(&exact_attach);
    assert_eq!(rearmed_id, id);
    assert_eq!(rearmed_template, template);
    let promoted = core.handle(EngineMsg::SignerCompleted(
        id,
        rearmed_generation,
        Ok(rearmed_template.sign_with_keys(&a).unwrap()),
    ));
    assert!(all_row_deltas(&promoted).iter().any(|delta| matches!(
        delta,
        RowDelta::Updated(row)
            if row.id() == expected_id
                && matches!(row.signature(), nmp::RowSignature::Signed(_))
                && row.signed_event().is_some_and(|event| event.verify().is_ok())
    )));
    for handle in [first_handle, second_handle] {
        let same_id_updates = promoted
            .iter()
            .filter_map(|effect| match effect {
                Effect::EmitRows(candidate, deltas, _) if *candidate == handle => Some(deltas),
                _ => None,
            })
            .flatten()
            .filter(|delta| matches!(delta, RowDelta::Updated(row) if row.id() == expected_id))
            .count();
        assert_eq!(
            same_id_updates, 1,
            "the exact signer promotes the row once for observation {handle:?}"
        );
    }
}

// ---- explicit per-write identity (#47) -----------------------------------

/// #47 falsifier (a) at the reducer level: an explicit
/// `Identity::Explicit(B)` on a builder is accepted and
/// signer-requested AS B while A stays the current account -- and a plain
/// default publish immediately after still roots on A, proving naming B
/// changed exactly one write and not the engine's identity root.
#[test]
fn an_explicit_identity_selects_a_secondary_author_and_pins_it_through_signing() {
    let a = Keys::generate();
    let b = Keys::generate();
    let mut core = new_core(FixtureRoutingFacts::new());
    activate(&mut core, &a);

    let as_b = draft(47, "published as b while a is current");
    let effects = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(as_b),
        routing: WriteRouting::Auto,
        identity: Identity::Explicit(b.public_key()),
        correlation: None,
    }));
    assert!(matches!(effects.first(), Some(Effect::WriteAccepted(..))));
    let (id, generation, template) = find_sign_request(&effects);
    assert_eq!(
        template.pubkey,
        b.public_key(),
        "the sign request must target the override identity, not the current account"
    );
    let signed = template.sign_with_keys(&b).unwrap();
    let expected_id = signed.id;
    assert!(signed.verify().is_ok());
    let effects = core.handle(EngineMsg::SignerCompleted(id, generation, Ok(signed)));
    assert!(
        effects.iter().any(|effect| matches!(
            effect,
            Effect::EmitReceipt(rid, WriteFact::Signing(SigningState::Signed { event_id }))
                if *rid == id && *event_id == expected_id
        )),
        "the frozen B-authored body must promote to Signed under B's key"
    );

    // Naming B never moved the engine's identity root: a default
    // (`Identity::Active`) publish is still accepted and roots on A.
    let effects = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(draft(48, "default path still roots on a")),
        routing: WriteRouting::Auto,
        identity: Identity::Active,
        correlation: None,
    }));
    assert!(matches!(effects.first(), Some(Effect::WriteAccepted(..))));
}

/// #47 falsifier (b), restated for a payload that cannot state an author.
/// The "draft author disagrees with the current account" refusal is gone --
/// not weakened, DELETED, because a builder has no author field for the
/// current account to disagree with. What survives is the half that is still
/// a refusal: `Active` names an account, and an instruction that cannot
/// resolve is a refusal, not a parked hope.
#[test]
fn a_builder_publishes_as_the_current_account_and_refuses_when_there_is_none() {
    let a = Keys::generate();
    let mut core = new_core(FixtureRoutingFacts::new());
    activate(&mut core, &a);

    let effects = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(draft(1, "as whoever is current")),
        routing: WriteRouting::Auto,
        identity: Identity::Active,
        correlation: None,
    }));
    assert!(
        matches!(effects.first(), Some(Effect::WriteAccepted(..))),
        "a kind and content, published as the current account, is the whole story"
    );
    let (_, _, template) = find_sign_request(&effects);
    assert_eq!(
        template.pubkey,
        a.public_key(),
        "the author the app never stated is the account it is logged in as"
    );

    core.handle(EngineMsg::SetActivePubkey(None));
    let effects = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(draft(2, "logged out, no identity named")),
        routing: WriteRouting::Auto,
        identity: Identity::Active,
        correlation: None,
    }));
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::PublishFailed(PublishError::NoCurrentAccount)]
        ),
        "nothing is pinned, so nothing may park -- got {effects:?}"
    );
    assert!(!effects
        .iter()
        .any(|effect| matches!(effect, Effect::RequestSign(..))));
}

/// #47 falsifier (c), split by where an author can come from. On a builder
/// the mismatch class is UNREPRESENTABLE -- an override cannot contradict an
/// author the payload has no field for -- so it simply selects. On a signed
/// event the author is frozen in the bytes, so the check survives verbatim
/// as a check: naming anybody else has no resolution that honours both
/// statements, and fails closed pre-acceptance.
#[test]
fn identity_selects_on_a_builder_and_may_only_restate_on_a_signed_event() {
    let a = Keys::generate();
    let b = Keys::generate();
    let mut core = new_core(FixtureRoutingFacts::new());
    activate(&mut core, &a);

    let effects = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(draft(1, "as b, while a is current")),
        routing: WriteRouting::Auto,
        identity: Identity::Explicit(b.public_key()),
        correlation: None,
    }));
    assert!(matches!(effects.first(), Some(Effect::WriteAccepted(..))));
    let (_, _, template) = find_sign_request(&effects);
    assert_eq!(
        template.pubkey,
        b.public_key(),
        "the named identity is the only source of a builder's author"
    );

    // Signed event authored by A, identity naming B: still a contradiction.
    let signed = signed_draft(&draft(2, "signed by a"), &a);
    let effects = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Signed(signed),
        routing: WriteRouting::Auto,
        identity: Identity::Explicit(b.public_key()),
        correlation: None,
    }));
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::PublishFailed(
                PublishError::IdentityContradictsSignedAuthor { identity, author }
            )] if *identity == b.public_key() && *author == a.public_key()
        ),
        "a contradiction has no correct resolution -- got {effects:?}"
    );
    assert!(!effects
        .iter()
        .any(|effect| matches!(effect, Effect::PublishEvent(..))));
}

#[test]
fn relay_rejection_after_promotion_does_not_retract_the_signed_row() {
    let a = Keys::generate();
    let relay = RelayUrl::parse("wss://write.example.com").unwrap();
    let dir = FixtureRoutingFacts::new().with_outbound_routes(a.public_key(), [relay.clone()]);
    let mut core = new_core(dir);
    core.handle(EngineMsg::RelayConnected(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        signer_session(&relay, a.public_key()),
    ));
    let signed = signed_draft(&draft(1, "signed cache truth"), &a);
    core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Signed(signed.clone()),
        routing: WriteRouting::Auto,
        identity: Identity::Active,
        correlation: None,
    }));
    let rejected = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        signer_session(&relay, signed.pubkey),
        RelayFrame::from(RelayMessage::ok(signed.id, false, "policy rejection")),
    ));
    assert!(!all_row_deltas(&rejected)
        .iter()
        .any(|delta| matches!(delta, RowDelta::Removed(id) if *id == signed.id)));
    let fresh = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &a.public_key().to_hex(),
    )));
    assert!(all_row_deltas(&fresh)
        .iter()
        .any(|delta| matches!(delta, RowDelta::Added(row) if row.id() == signed.id)));
}

#[test]
fn cancelling_newest_never_restores_a_destroyed_local_predecessor_chain() {
    let a = Keys::generate();
    let mut core = new_core(FixtureRoutingFacts::new());
    activate(&mut core, &a);
    core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[0],
        &a.public_key().to_hex(),
    )));

    let base = UnsignedEvent::new(
        a.public_key(),
        Timestamp::from(1),
        Kind::Metadata,
        Vec::new(),
        "base",
    )
    .sign_with_keys(&a)
    .unwrap();
    let base_id = base.id;
    core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Signed(base.clone()),
        routing: WriteRouting::Auto,
        identity: Identity::Active,
        correlation: None,
    }));

    let middle = nmp_grammar::EventBuilder::new(Kind::Metadata)
        .content("middle")
        .created_at(Timestamp::from(2));
    let middle_id = signed_draft(&middle, &a).id;
    let middle_effects = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(middle.clone()),
        routing: WriteRouting::Auto,
        identity: Identity::Active,
        correlation: None,
    }));
    let (middle_receipt, _, _) = find_sign_request(&middle_effects);

    let newest = nmp_grammar::EventBuilder::new(Kind::Metadata)
        .content("newest")
        .created_at(Timestamp::from(3));
    let newest_id = signed_draft(&newest, &a).id;
    let newest_effects = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(newest.clone()),
        routing: WriteRouting::Auto,
        identity: Identity::Active,
        correlation: None,
    }));
    let (newest_receipt, _, _) = find_sign_request(&newest_effects);

    let older_cancel = core.handle(EngineMsg::CancelWrite(middle_receipt));
    assert!(!all_row_deltas(&older_cancel).iter().any(|delta| {
        matches!(delta, RowDelta::Removed(id) if *id == newest_id)
            || matches!(delta, RowDelta::Added(row) if row.id() == middle_id)
    }));

    let newest_cancel = core.handle(EngineMsg::CancelWrite(newest_receipt));
    assert!(all_row_deltas(&newest_cancel)
        .iter()
        .any(|delta| matches!(delta, RowDelta::Removed(id) if *id == newest_id)));
    assert!(!all_row_deltas(&newest_cancel)
        .iter()
        .any(|delta| matches!(delta, RowDelta::Added(row) if row.id() == middle_id)));
    assert!(!all_row_deltas(&newest_cancel)
        .iter()
        .any(|delta| matches!(delta, RowDelta::Added(row) if row.id() == base_id)));
    let fresh = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[0],
        &a.public_key().to_hex(),
    )));
    assert!(!all_row_deltas(&fresh)
        .iter()
        .any(|delta| matches!(delta, RowDelta::Added(row) if row.id() == base_id)));
    assert!(!all_row_deltas(&fresh)
        .iter()
        .any(|delta| matches!(delta, RowDelta::Added(row) if row.id() == middle_id)));
}

#[test]
fn expired_local_acceptance_is_refused_before_custody_and_retains_nothing() {
    let a = Keys::generate();
    let relay = RelayUrl::parse("wss://write.example.com").unwrap();
    let dir = FixtureRoutingFacts::new().with_outbound_routes(a.public_key(), [relay]);
    let mut core = new_core(dir);
    core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &a.public_key().to_hex(),
    )));
    core.handle(EngineMsg::Tick(Timestamp::from(200)));
    let expired = nmp_resolver::testkit::expiring_kind1(&a, "expired", 100, 150);
    let effects = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Signed(expired),
        routing: WriteRouting::Auto,
        identity: Identity::Active,
        correlation: None,
    }));
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::PublishFailed(PublishError::AlreadyExpired)]
        ),
        "expired work must be rejected before custody -- got {effects:?}"
    );
    assert!(
        core.publish_queue_entries(None, u8::MAX)
            .unwrap()
            .is_empty(),
        "an attempt rejected before custody must not allocate a receipt"
    );
}

#[test]
fn exact_duplicate_intents_get_distinct_store_ids_and_one_promotion_advances_both() {
    let a = Keys::generate();
    let relay = RelayUrl::parse("wss://write.example.com").unwrap();
    let dir = FixtureRoutingFacts::new().with_outbound_routes(a.public_key(), [relay]);
    let mut core = new_core(dir);
    activate(&mut core, &a);
    let template = draft(1, "same body");

    let first = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(template.clone()),
        routing: WriteRouting::Auto,
        identity: Identity::Active,
        correlation: None,
    }));
    let (first_id, first_generation, first_template) = find_sign_request(&first);
    let second = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(template.clone()),
        routing: WriteRouting::Auto,
        identity: Identity::Active,
        correlation: None,
    }));
    let (second_id, second_generation, second_template) = find_sign_request(&second);
    assert_ne!(
        first_id, second_id,
        "each accepted obligation owns one store id"
    );

    let signed = first_template.sign_with_keys(&a).unwrap();
    let effects = core.handle(EngineMsg::SignerCompleted(
        first_id,
        first_generation,
        Ok(signed.clone()),
    ));
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::EmitReceipt(id, WriteFact::Signing(SigningState::Signed { event_id }))
            if *id == first_id && *event_id == signed.id
    )));
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::EmitReceipt(id, WriteFact::Signing(SigningState::Signed { event_id }))
            if *id == second_id && *event_id == signed.id
    )));

    // The co-owner was atomically promoted by the first completion; its
    // delayed signer result is ignored and cannot publish a second time.
    let delayed = second_template.sign_with_keys(&a).unwrap();
    let effects = core.handle(EngineMsg::SignerCompleted(
        second_id,
        second_generation,
        Ok(delayed),
    ));
    assert!(effects.is_empty());
}

#[test]
fn duplicate_coowners_keep_independent_routes_and_terminal_receipts() {
    let a = Keys::generate();
    let ack = RelayUrl::parse("wss://ack.example.com").unwrap();
    let nack = RelayUrl::parse("wss://nack.example.com").unwrap();
    let drop_relay = RelayUrl::parse("wss://drop.example.com").unwrap();
    let mut core = new_core(FixtureRoutingFacts::new());
    activate(&mut core, &a);
    connect_signer(&mut core, 0, &ack, a.public_key());
    connect_signer(&mut core, 1, &nack, a.public_key());
    connect_signer(&mut core, 2, &drop_relay, a.public_key());
    authenticate_signer(&mut core, 0, &ack, &a);
    authenticate_signer(&mut core, 1, &nack, &a);
    authenticate_signer(&mut core, 2, &drop_relay, &a);
    let template = draft(1, "same bytes, separate obligations");

    let first = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(template.clone()),
        routing: WriteRouting::Explicit(vec![ack.clone(), drop_relay.clone()]),
        identity: Identity::Active,
        correlation: None,
    }));
    let (id_a, generation_a, to_sign) = find_sign_request(&first);
    let second = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(template.clone()),
        routing: WriteRouting::Explicit(vec![nack.clone()]),
        identity: Identity::Active,
        correlation: None,
    }));
    let (id_b, _, _) = find_sign_request(&second);
    let signed = to_sign.sign_with_keys(&a).unwrap();
    let routed = core.handle(EngineMsg::SignerCompleted(
        id_a,
        generation_a,
        Ok(signed.clone()),
    ));
    assert!(routed.iter().any(
        |effect| matches!(effect, Effect::PublishEvent(session, event, _)
            if session == &signer_session(&ack, event.pubkey))
    ));
    assert!(routed.iter().any(
        |effect| matches!(effect, Effect::PublishEvent(session, event, _)
            if session == &signer_session(&drop_relay, event.pubkey))
    ));
    assert!(routed.iter().any(
        |effect| matches!(effect, Effect::PublishEvent(session, event, _)
            if session == &signer_session(&nack, event.pubkey))
    ));
    mark_written(&mut core, &routed, &ack);
    mark_written(&mut core, &routed, &nack);
    mark_written(&mut core, &routed, &drop_relay);

    let acked = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        signer_session(&ack, signed.pubkey),
        RelayFrame::from(RelayMessage::ok(signed.id, true, "")),
    ));
    assert!(acked.iter().any(|effect| matches!(
        effect,
        Effect::EmitReceipt(id, WriteFact::Relay { relay, state: RelayState::Published, .. }) if *id == id_a && relay == &ack
    )));
    assert!(!acked
        .iter()
        .any(|effect| matches!(effect, Effect::EmitReceipt(id, _) if *id == id_b)));

    let nacked = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 1,
            generation: 1,
        },
        signer_session(&nack, signed.pubkey),
        RelayFrame::from(RelayMessage::ok(signed.id, false, "no")),
    ));
    assert!(nacked.iter().any(|effect| matches!(
        effect,
        Effect::EmitReceipt(id, WriteFact::Relay { relay, state: RelayState::Rejected { reason: _ }, .. }) if *id == id_b && relay == &nack
    )));

    let dropped = core.handle(EngineMsg::RelayDisconnected(
        RelayHandle {
            slot: 2,
            generation: 1,
        },
        signer_session(&drop_relay, signed.pubkey),
        DisconnectReason::Error,
    ));
    assert!(!dropped.iter().any(
        |effect| matches!(effect, Effect::EmitReceipt(id, WriteFact::Relay { relay: _, state: RelayState::GaveUp, .. }) if *id == id_a)
    ));
    assert!(
        core.next_deadline().expect("deadline peek").is_some(),
        "durable disconnect arms retry eligibility"
    );
}

#[test]
fn relay_signature_satisfies_all_pending_coowners_and_late_signers_are_ignored() {
    let a = Keys::generate();
    let source = RelayUrl::parse("wss://source.example.com").unwrap();
    let out = RelayUrl::parse("wss://out.example.com").unwrap();
    let dir = FixtureRoutingFacts::new().with_outbound_routes(a.public_key(), [out.clone()]);
    let mut core = new_core(dir);
    activate(&mut core, &a);
    connect_signer(&mut core, 0, &source, a.public_key());
    connect_signer(&mut core, 1, &out, a.public_key());
    authenticate_signer(&mut core, 0, &source, &a);
    authenticate_signer(&mut core, 1, &out, &a);
    let template = draft(1, "relay wins signing race");
    let first = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(template.clone()),
        routing: WriteRouting::Auto,
        identity: Identity::Active,
        correlation: None,
    }));
    let (id_a, generation_a, signer_a) = find_sign_request(&first);
    let second = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(template.clone()),
        routing: WriteRouting::Auto,
        identity: Identity::Active,
        correlation: None,
    }));
    let (id_b, generation_b, signer_b) = find_sign_request(&second);
    let signed = signer_a.clone().sign_with_keys(&a).unwrap();
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        signer_session(&source, signed.pubkey),
        event_frame("unsolicited", signed.clone()),
    ));
    for id in [id_a, id_b] {
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::EmitReceipt(receipt, WriteFact::Signing(SigningState::Signed { event_id }))
                if *receipt == id && *event_id == signed.id
        )));
    }
    assert_eq!(
        effects
            .iter()
            .filter(
                |effect| matches!(effect, Effect::PublishEvent(session, event, _)
                if session == &signer_session(&out, event.pubkey))
            )
            .count(),
        1,
        "the per-relay cap admits only one co-owner lane at a time"
    );
    mark_written(&mut core, &effects, &out);
    let advanced = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 1,
            generation: 1,
        },
        signer_session(&out, signed.pubkey),
        RelayFrame::from(RelayMessage::ok(signed.id, true, "")),
    ));
    assert_eq!(
        advanced
            .iter()
            .filter(
                |effect| matches!(effect, Effect::PublishEvent(session, event, _)
                if session == &signer_session(&out, event.pubkey))
            )
            .count(),
        1,
        "terminalizing the first lane wakes the next fair lane"
    );
    assert!(core
        .handle(EngineMsg::SignerCompleted(
            id_a,
            generation_a,
            Ok(signer_a.sign_with_keys(&a).unwrap()),
        ))
        .is_empty());
    assert!(core
        .handle(EngineMsg::SignerCompleted(
            id_b,
            generation_b,
            Ok(signer_b.sign_with_keys(&a).unwrap()),
        ))
        .is_empty());
}

#[test]
fn repeated_signer_notifications_never_start_concurrent_operations() {
    let a = Keys::generate();
    let mut core = new_core(FixtureRoutingFacts::new());
    activate(&mut core, &a);
    let published = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(draft(1, "one operation")),
        routing: WriteRouting::Auto,
        identity: Identity::Active,
        correlation: None,
    }));
    let (id, generation, template) = find_sign_request(&published);
    assert!(core
        .handle(EngineMsg::SignerAttached(a.public_key()))
        .is_empty());
    assert!(core
        .handle(EngineMsg::SignerAttached(a.public_key()))
        .is_empty());

    core.handle(EngineMsg::SignerUnavailable(id, generation));
    let rearmed = core.handle(EngineMsg::SignerAttached(a.public_key()));
    assert_eq!(
        rearmed
            .iter()
            .filter(|effect| matches!(effect, Effect::RequestSign(..)))
            .count(),
        1
    );
    let (_, next_generation, _) = find_sign_request(&rearmed);
    assert!(next_generation > generation);
    let signed = template.sign_with_keys(&a).unwrap();
    assert!(core
        .handle(EngineMsg::SignerCompleted(
            id,
            generation,
            Ok(signed.clone())
        ))
        .is_empty());
    assert!(core
        .handle(EngineMsg::SignerAttached(a.public_key()))
        .is_empty());
    let completed = core.handle(EngineMsg::SignerCompleted(id, next_generation, Ok(signed)));
    assert!(completed.iter().any(|effect| matches!(
        effect,
        Effect::EmitReceipt(rid, WriteFact::Signing(SigningState::Signed { event_id: _ })) if *rid == id
    )));
}

#[test]
fn retryable_signer_errors_retain_and_rearm_the_exact_write() {
    for error in [
        nmp_signer::SignerError::Unavailable,
        nmp_signer::SignerError::Timeout,
        nmp_signer::SignerError::Disconnected,
    ] {
        let a = Keys::generate();
        let mut core = new_core(FixtureRoutingFacts::new());
        activate(&mut core, &a);
        let published = core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::Event(draft(1, "survives signer loss")),
            routing: WriteRouting::Auto,
            identity: Identity::Active,
            correlation: None,
        }));
        let (id, generation, frozen) = find_sign_request(&published);

        let waiting = core.handle(EngineMsg::SignerCompleted(id, generation, Err(error)));
        assert!(waiting.iter().any(|effect| matches!(
            effect,
            Effect::EmitReceipt(rid, WriteFact::Signing(SigningState::AwaitingSigner { pubkey }))
                if *rid == id && *pubkey == a.public_key()
        )));
        assert!(waiting.iter().any(|effect| matches!(
            effect,
            Effect::RearmSignerIfAvailable(pubkey) if *pubkey == a.public_key()
        )));
        assert_eq!(
            receipt_statuses(&waiting).last(),
            Some(&WriteFact::Signing(SigningState::AwaitingSigner {
                pubkey: a.public_key()
            }))
        );

        let rearmed = core.handle(EngineMsg::SignerAttached(a.public_key()));
        let (rearmed_id, next_generation, rearmed_frozen) = find_sign_request(&rearmed);
        assert_eq!(rearmed_id, id);
        assert!(next_generation > generation);
        assert_eq!(rearmed_frozen.pubkey, frozen.pubkey);
        assert_eq!(rearmed_frozen.created_at, frozen.created_at);
        assert_eq!(rearmed_frozen.kind, frozen.kind);
        assert_eq!(rearmed_frozen.tags, frozen.tags);
        assert_eq!(rearmed_frozen.content, frozen.content);
        assert_eq!(
            rearmed_frozen.id,
            Some(frozen.sign_with_keys(&a).unwrap().id),
            "reattachment must use the canonical id frozen at acceptance",
        );
    }
}

#[test]
fn terminal_signer_errors_compensate_the_write() {
    for error in [
        nmp_signer::SignerError::Rejected("user denied".to_string()),
        nmp_signer::SignerError::InvalidResponse("body mismatch".to_string()),
    ] {
        let a = Keys::generate();
        let mut core = new_core(FixtureRoutingFacts::new());
        activate(&mut core, &a);
        let published = core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::Event(draft(1, "terminal signer answer")),
            routing: WriteRouting::Auto,
            identity: Identity::Active,
            correlation: None,
        }));
        let (id, generation, _) = find_sign_request(&published);

        let failed = core.handle(EngineMsg::SignerCompleted(id, generation, Err(error)));
        assert!(failed.iter().any(|effect| matches!(
            effect,
            Effect::EmitReceipt(rid, WriteFact::Signing(SigningState::Refused { .. }))
                if *rid == id
        )));
        assert!(failed.iter().any(|effect| matches!(
            effect,
            Effect::EmitReceipt(
                rid,
                WriteFact::Outcome(WriteOutcome::NotSent(NotSentReason::SignerRefused))
            ) if *rid == id
        )));
        assert!(core
            .handle(EngineMsg::SignerAttached(a.public_key()))
            .iter()
            .all(|effect| !matches!(effect, Effect::RequestSign(..))));
    }
}

#[test]
fn compensation_persistence_failure_is_nonterminal_and_retryable() {
    let a = Keys::generate();
    let mut core = EngineCore::new(FailOnceCompensationStore::new(), 10);
    activate(&mut core, &a);
    core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &a.public_key().to_hex(),
    )));
    let published = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(draft(1, "must remain pending")),
        routing: WriteRouting::Auto,
        identity: Identity::Active,
        correlation: None,
    }));
    let (id, generation, template) = find_sign_request(&published);
    let event_id = template.sign_with_keys(&a).unwrap().id;

    let failed_compensation = core.handle(EngineMsg::SignerCompleted(
        id,
        generation,
        Err(nmp_signer::SignerError::Rejected(
            "terminal signer decision".to_string(),
        )),
    ));
    assert!(failed_compensation.is_empty(), "no terminal fact committed");
    assert!(
        receipt_statuses(&published).is_empty(),
        "the accepted write has learned nothing yet -- least of all a terminal"
    );
    let fresh = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &a.public_key().to_hex(),
    )));
    assert!(all_row_deltas(&fresh)
        .iter()
        .any(|delta| matches!(delta, RowDelta::Added(row) if row.id() == event_id)));

    let (outcome, retried) = core.cancel_write(id);
    assert_eq!(
        outcome,
        Ok(nmp::mechanism::publish_queue::CancelWriteOutcome::Cancelled)
    );
    assert!(retried.iter().any(
        |effect| matches!(effect, Effect::EmitReceipt(rid, WriteFact::Outcome(WriteOutcome::NotSent(NotSentReason::Cancelled))) if *rid == id)
    ));
    assert!(all_row_deltas(&retried)
        .iter()
        .any(|delta| matches!(delta, RowDelta::Removed(removed) if *removed == event_id)));
}

#[test]
fn explicit_cancellation_persistence_failure_keeps_the_obligation_live_until_retry() {
    let a = Keys::generate();
    let mut core = EngineCore::new(FailOnceCompensationStore::new(), 10);
    activate(&mut core, &a);
    core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &a.public_key().to_hex(),
    )));
    let published = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(draft(2, "cancel must commit first")),
        routing: WriteRouting::Auto,
        identity: Identity::Active,
        correlation: None,
    }));
    let (id, _, template) = find_sign_request(&published);
    let event_id = template.sign_with_keys(&a).unwrap().id;

    let (refused, effects) = core.cancel_write(id);
    assert!(matches!(
        refused,
        Err(nmp::mechanism::publish_queue::CancelWriteError::PersistenceFailed {
            receipt_id,
            reason,
        }) if receipt_id == id && reason.contains("injected compensation failure")
    ));
    assert!(
        effects.is_empty(),
        "a refused cancel must emit no terminal fact"
    );
    assert!(
        receipt_statuses(&published).is_empty(),
        "the accepted write has learned nothing yet -- least of all a terminal"
    );

    let fresh = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &a.public_key().to_hex(),
    )));
    assert!(all_row_deltas(&fresh)
        .iter()
        .any(|delta| matches!(delta, RowDelta::Added(row) if row.id() == event_id)));

    let (committed, effects) = core.cancel_write(id);
    assert_eq!(
        committed,
        Ok(nmp::mechanism::publish_queue::CancelWriteOutcome::Cancelled)
    );
    assert!(effects.iter().any(
        |effect| matches!(effect, Effect::EmitReceipt(rid, WriteFact::Outcome(WriteOutcome::NotSent(NotSentReason::Cancelled))) if *rid == id)
    ));
    assert!(all_row_deltas(&effects)
        .iter()
        .any(|delta| matches!(delta, RowDelta::Removed(removed) if *removed == event_id)));
    let mut statuses = receipt_statuses(&published);
    statuses.extend(receipt_statuses(&effects));
    assert_eq!(
        statuses,
        [WriteFact::Outcome(WriteOutcome::NotSent(
            NotSentReason::Cancelled
        ))]
    );
}

/// #52 Q2 smoking gun: `EngineCore::on_publish` is the ONE place every
/// publish converges (FFI, direct-Rust, `nmp-bdd`'s `EngineThread`), so a
/// `WritePayload::Signed` whose content was tampered with after signing
/// (id/sig stale relative to the new content) must be rejected there: the
/// call itself refuses, nothing is taken into custody, and no
/// `Effect::PublishEvent` is produced -- regardless of caller, with no FFI
/// verify layer anywhere in the loop.
#[test]
fn direct_publish_of_forged_signed_event_is_rejected_before_acceptance() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureRoutingFacts::new().with_outbound_routes(a.public_key(), [relay0.clone()]);
    let mut core = new_core(dir);
    connect_signer(&mut core, 0, &relay0, a.public_key());

    let genuine = signed_draft(&draft(1, "genuine content"), &a);
    // Forge: reuse the genuine id/signature but swap in different content --
    // exactly the "reconstructed from caller-supplied fields verbatim"
    // shape the FFI boundary's own `signed_event_from_ffi` guards against,
    // now driven straight through `Handle::publish` with no FFI in the loop.
    let forged = nostr::Event::new(
        genuine.id,
        genuine.pubkey,
        genuine.created_at,
        genuine.kind,
        genuine.tags.clone(),
        "forged content -- attacker tampered after signing",
        genuine.sig,
    );
    assert!(
        forged.verify().is_err(),
        "test fixture sanity: the forged event must not verify"
    );

    let effects = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Signed(forged),
        routing: WriteRouting::Auto,
        identity: Identity::Active,
        correlation: None,
    }));

    assert!(
        matches!(
            effects.as_slice(),
            [Effect::PublishFailed(PublishError::SignatureInvalid { .. })]
        ),
        "a forged Signed publish must refuse the call as the ONLY effect -- got {effects:?}"
    );
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::PublishEvent(..))),
        "a forged Signed publish must never produce Effect::PublishEvent"
    );
    assert!(
        receipt_statuses(&effects).is_empty(),
        "nothing was taken into custody, so there is no receipt stream to say anything"
    );
}

/// Companion to the forged-event smoking gun: a properly-signed `Signed`
/// payload is unaffected by the acceptance-boundary verify and flows to
/// `Effect::PublishEvent` exactly as before -- no `RequestSign` (VISION P:
/// a caller that already holds a valid signature skips signing entirely).
#[test]
fn direct_publish_of_valid_signed_event_still_publishes() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureRoutingFacts::new().with_outbound_routes(a.public_key(), [relay0.clone()]);
    let mut core = new_core(dir);
    connect_signer(&mut core, 0, &relay0, a.public_key());
    authenticate_signer(&mut core, 0, &relay0, &a);

    let genuine = signed_draft(&draft(1, "genuine content"), &a);

    let effects = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Signed(genuine.clone()),
        routing: WriteRouting::Auto,
        identity: Identity::Active,
        correlation: None,
    }));

    assert!(
        matches!(effects.first(), Some(Effect::WriteAccepted(..))),
        "a valid Signed publish must still be taken into custody first"
    );
    assert!(
        !effects.iter().any(|e| matches!(e, Effect::RequestSign(..))),
        "an already-signed payload must never request the signer"
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::PublishEvent(r, ev, _)
                if r == &signer_session(&relay0, genuine.pubkey) && ev.id == genuine.id)),
        "a valid Signed publish must still reach the wire -- got {effects:?}"
    );
}
