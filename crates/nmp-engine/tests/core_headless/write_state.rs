use super::*;

// ---- write projection and lifecycle ------------------------------------

#[test]
fn durable_pending_row_is_visible_before_signer_and_tamper_compensates() {
    let a = Keys::generate();
    let relay = RelayUrl::parse("wss://write.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay]);
    let mut core = new_core(dir);
    activate(&mut core, &a);
    let row_sink = CapturingSink::default();
    core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(row_sink),
    ));

    let receipt_sink = CapturingReceiptSink::default();
    let effects = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(unsigned(&a, 10, "accepted body")),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: None,
        },
        Box::new(receipt_sink.clone()),
    ));
    let (id, generation, accepted_template) = find_sign_request(&effects);
    let accepted_id = accepted_template.clone().sign_with_keys(&a).unwrap().id;
    assert!(all_row_deltas(&effects)
        .iter()
        .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == accepted_id)));
    assert!(matches!(
        receipt_sink.0.lock().unwrap().as_slice(),
        [WriteStatus::Accepted]
    ));

    let tampered = unsigned(&a, 10, "different signer output")
        .sign_with_keys(&a)
        .unwrap();
    let effects = core.handle(EngineMsg::SignerCompleted(id, generation, Ok(tampered)));
    assert!(!effects
        .iter()
        .any(|effect| matches!(effect, Effect::PublishEvent(..))));
    assert!(all_row_deltas(&effects)
        .iter()
        .any(|delta| matches!(delta, RowDelta::Removed(event_id) if *event_id == accepted_id)));
    assert!(matches!(
        receipt_sink.0.lock().unwrap().last(),
        Some(WriteStatus::Failed(_))
    ));
}

#[test]
fn cancellation_restores_replaceable_predecessor_through_query_reactivity() {
    let a = Keys::generate();
    let relay = RelayUrl::parse("wss://write.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay]);
    let mut core = new_core(dir);
    activate(&mut core, &a);
    core.handle(EngineMsg::Subscribe(
        literal_query(&[0], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));

    let older_unsigned = UnsignedEvent::new(
        a.public_key(),
        Timestamp::from(1),
        Kind::Metadata,
        Vec::new(),
        "older",
    );
    let older = older_unsigned.sign_with_keys(&a).unwrap();
    core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Signed(older.clone()),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: None,
        },
        Box::new(CapturingReceiptSink::default()),
    ));

    let newer_unsigned = UnsignedEvent::new(
        a.public_key(),
        Timestamp::from(2),
        Kind::Metadata,
        Vec::new(),
        "newer",
    );
    let newer_id = newer_unsigned.clone().sign_with_keys(&a).unwrap().id;
    let cancel_sink = CapturingReceiptSink::default();
    let effects = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(newer_unsigned),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: None,
        },
        Box::new(cancel_sink.clone()),
    ));
    let (newer_receipt, _, _) = find_sign_request(&effects);
    assert!(all_row_deltas(&effects)
        .iter()
        .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == newer_id)));
    assert!(all_row_deltas(&effects)
        .iter()
        .any(|delta| matches!(delta, RowDelta::Removed(id) if *id == older.id)));

    let (outcome, effects) = core.cancel_write(newer_receipt);
    assert_eq!(
        outcome,
        Ok(nmp_engine::outbox::CancelWriteOutcome::Cancelled)
    );
    assert_eq!(
        cancel_sink.0.lock().unwrap().last(),
        Some(&WriteStatus::Cancelled)
    );
    assert!(all_row_deltas(&effects)
        .iter()
        .any(|delta| matches!(delta, RowDelta::Removed(id) if *id == newer_id)));
    assert!(all_row_deltas(&effects)
        .iter()
        .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == older.id)));
}

#[test]
fn cancellation_outcomes_are_typed_idempotent_and_late_signers_are_inert() {
    let a = Keys::generate();
    let relay = RelayUrl::parse("wss://write.example.com").unwrap();
    let mut core =
        new_core(FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay.clone()]));
    activate(&mut core, &a);

    let sink = CapturingReceiptSink::default();
    let published = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(unsigned(&a, 10, "cancel typed")),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: None,
        },
        Box::new(sink.clone()),
    ));
    let (receipt, generation, template) = find_sign_request(&published);
    let signed = template.sign_with_keys(&a).unwrap();

    assert_eq!(
        core.cancel_write(receipt).0,
        Ok(nmp_engine::outbox::CancelWriteOutcome::Cancelled)
    );
    assert_eq!(
        core.cancel_write(receipt).0,
        Ok(nmp_engine::outbox::CancelWriteOutcome::Cancelled)
    );
    assert!(core
        .handle(EngineMsg::SignerCompleted(receipt, generation, Ok(signed)))
        .is_empty());
    assert_eq!(
        sink.0.lock().unwrap().as_slice(),
        [WriteStatus::Accepted, WriteStatus::Cancelled]
    );
    assert!(matches!(
        core.cancel_write(ReceiptId(u64::MAX)).0,
        Err(nmp_engine::outbox::CancelWriteError::UnknownReceipt { .. })
    ));

    let signed_sink = CapturingReceiptSink::default();
    let signed_event = unsigned(&a, 11, "already signed")
        .sign_with_keys(&a)
        .unwrap();
    let signed_publish = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Signed(signed_event.clone()),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: None,
        },
        Box::new(signed_sink),
    ));
    let signed_receipt = signed_publish
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitReceipt(id, WriteStatus::Accepted) => Some(*id),
            _ => None,
        })
        .unwrap();
    assert!(matches!(
        core.cancel_write(signed_receipt).0,
        Err(nmp_engine::outbox::CancelWriteError::AlreadySigned {
            event_id: id,
            ..
        }) if id == signed_event.id
    ));
}

#[test]
fn signer_unavailable_keeps_accepted_row_visible() {
    let a = Keys::generate();
    let mut core = new_core(FixtureDirectory::new());
    activate(&mut core, &a);
    core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let sink = CapturingReceiptSink::default();
    let effects = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(unsigned(&a, 1, "awaiting signer")),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: None,
        },
        Box::new(sink.clone()),
    ));
    let (id, generation, template) = find_sign_request(&effects);
    let expected_id = template.sign_with_keys(&a).unwrap().id;
    let effects = core.handle(EngineMsg::SignerUnavailable(id, generation));
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::EmitReceipt(rid, WriteStatus::AwaitingCapability { pubkey })
            if *rid == id && *pubkey == a.public_key()
    )));
    let fresh = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    assert!(all_row_deltas(&fresh)
        .iter()
        .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == expected_id)));
}

// ---- explicit per-write identity override (#47) --------------------------

/// #47 falsifier (a) at the reducer level: an explicit
/// `identity_override: Some(B)` on a B-authored draft is accepted and
/// signer-requested AS B while A stays the active account -- and a plain
/// default publish immediately after still roots on A, proving the override
/// changed exactly one write and not the engine's identity root.
#[test]
fn identity_override_accepts_secondary_author_and_pins_it_through_signing() {
    let a = Keys::generate();
    let b = Keys::generate();
    let mut core = new_core(FixtureDirectory::new());
    activate(&mut core, &a);

    let draft = unsigned(&b, 47, "published as b while a is active");
    let sink = CapturingReceiptSink::default();
    let effects = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(draft.clone()),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: Some(b.public_key()),
            correlation: None,
        },
        Box::new(sink.clone()),
    ));
    assert!(matches!(
        effects.first(),
        Some(Effect::EmitReceipt(_, WriteStatus::Accepted))
    ));
    let (id, generation, template) = find_sign_request(&effects);
    assert_eq!(
        template.pubkey,
        b.public_key(),
        "the sign request must target the override identity, not the active account"
    );
    let signed = template.sign_with_keys(&b).unwrap();
    let expected_id = signed.id;
    assert!(signed.verify().is_ok());
    let effects = core.handle(EngineMsg::SignerCompleted(id, generation, Ok(signed)));
    assert!(
        effects.iter().any(|effect| matches!(
            effect,
            Effect::EmitReceipt(rid, WriteStatus::Signed(event_id))
                if *rid == id && *event_id == expected_id
        )),
        "the frozen B-authored body must promote to Signed under B's key"
    );

    // The override never moved the engine's identity root: a default
    // (no-override) publish authored by A is still accepted.
    let default_sink = CapturingReceiptSink::default();
    let effects = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(unsigned(&a, 48, "default path still roots on a")),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: None,
        },
        Box::new(default_sink.clone()),
    ));
    assert!(matches!(
        effects.first(),
        Some(Effect::EmitReceipt(_, WriteStatus::Accepted))
    ));
    assert_eq!(
        default_sink.0.lock().unwrap().first(),
        Some(&WriteStatus::Accepted)
    );
}

/// #47 falsifier (b): the DEFAULT arm is byte-for-byte unchanged -- a
/// non-active author without an override still fails closed with the exact
/// pre-#47 messages, no `Accepted`, no sign request.
#[test]
fn default_publish_without_override_still_fails_closed_for_non_active_author() {
    let a = Keys::generate();
    let b = Keys::generate();
    let mut core = new_core(FixtureDirectory::new());
    activate(&mut core, &a);

    let sink = CapturingReceiptSink::default();
    let effects = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(unsigned(&b, 1, "no consent given")),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: None,
        },
        Box::new(sink.clone()),
    ));
    assert_eq!(
        sink.0.lock().unwrap().as_slice(),
        [WriteStatus::Failed(
            "unsigned draft author does not match current active account".to_string()
        )],
        "Failed must be the first and only status -- never Accepted"
    );
    assert!(!effects
        .iter()
        .any(|effect| matches!(effect, Effect::RequestSign(..))));

    core.handle(EngineMsg::SetActivePubkey(None));
    let logged_out = CapturingReceiptSink::default();
    let effects = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(unsigned(&b, 2, "logged out, no override")),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: None,
        },
        Box::new(logged_out.clone()),
    ));
    assert_eq!(
        logged_out.0.lock().unwrap().as_slice(),
        [WriteStatus::Failed(
            "unsigned publish requires an active account".to_string()
        )]
    );
    assert!(!effects
        .iter()
        .any(|effect| matches!(effect, Effect::RequestSign(..))));
}

/// #47 falsifier (c): an override that CONTRADICTS the draft's author fails
/// closed pre-acceptance for both payload variants -- the engine never
/// restamps a draft to satisfy an override, and no `Accepted` is ever
/// emitted for the contradiction.
#[test]
fn identity_override_author_mismatch_fails_closed_for_unsigned_and_signed() {
    let a = Keys::generate();
    let b = Keys::generate();
    let mut core = new_core(FixtureDirectory::new());
    activate(&mut core, &a);

    // Unsigned draft authored by A, override naming B: mismatch.
    let sink = CapturingReceiptSink::default();
    let effects = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(unsigned(&a, 1, "authored by a")),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: Some(b.public_key()),
            correlation: None,
        },
        Box::new(sink.clone()),
    ));
    assert_eq!(
        sink.0.lock().unwrap().as_slice(),
        [WriteStatus::Failed(format!(
            "identity override {} does not match the unsigned draft author {}",
            b.public_key(),
            a.public_key()
        ))],
        "the mismatch must be Failed-first-and-only, never Accepted"
    );
    assert!(!effects
        .iter()
        .any(|effect| matches!(effect, Effect::RequestSign(..))));

    // Signed event authored by A, override naming B: same contradiction.
    let signed = unsigned(&a, 2, "signed by a").sign_with_keys(&a).unwrap();
    let signed_sink = CapturingReceiptSink::default();
    let effects = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Signed(signed),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: Some(b.public_key()),
            correlation: None,
        },
        Box::new(signed_sink.clone()),
    ));
    assert_eq!(
        signed_sink.0.lock().unwrap().as_slice(),
        [WriteStatus::Failed(format!(
            "identity override {} does not match the signed event author {}",
            b.public_key(),
            a.public_key()
        ))]
    );
    assert!(!effects
        .iter()
        .any(|effect| matches!(effect, Effect::PublishEvent(..))));
}

#[test]
fn ephemeral_is_receipt_only_and_never_creates_a_pending_row() {
    let a = Keys::generate();
    let relay = RelayUrl::parse("wss://write.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay]);
    let mut core = new_core(dir);
    activate(&mut core, &a);
    core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let sink = CapturingReceiptSink::default();
    let effects = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(unsigned(&a, 1, "ephemeral")),
            durability: Durability::Ephemeral,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: None,
        },
        Box::new(sink.clone()),
    ));
    assert!(matches!(
        effects.first(),
        Some(Effect::EmitReceipt(_, WriteStatus::Accepted))
    ));
    assert!(all_row_deltas(&effects).is_empty());
    let fresh = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    assert!(all_row_deltas(&fresh).is_empty());
}

#[test]
fn relay_rejection_after_promotion_does_not_retract_the_signed_row() {
    let a = Keys::generate();
    let relay = RelayUrl::parse("wss://write.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay.clone()]);
    let mut core = new_core(dir);
    core.handle(EngineMsg::RelayConnected(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        signer_session(&relay, a.public_key()),
    ));
    let signed = unsigned(&a, 1, "signed cache truth")
        .sign_with_keys(&a)
        .unwrap();
    core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Signed(signed.clone()),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: None,
        },
        Box::new(CapturingReceiptSink::default()),
    ));
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
    let fresh = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    assert!(all_row_deltas(&fresh)
        .iter()
        .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == signed.id)));
}

#[test]
fn cancelling_displaced_pending_then_newest_never_resurrects_cancelled_row() {
    let a = Keys::generate();
    let mut core = new_core(FixtureDirectory::new());
    activate(&mut core, &a);
    core.handle(EngineMsg::Subscribe(
        literal_query(&[0], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));

    let base = UnsignedEvent::new(
        a.public_key(),
        Timestamp::from(1),
        Kind::Metadata,
        Vec::new(),
        "base",
    )
    .sign_with_keys(&a)
    .unwrap();
    core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Signed(base.clone()),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: None,
        },
        Box::new(CapturingReceiptSink::default()),
    ));

    let middle = UnsignedEvent::new(
        a.public_key(),
        Timestamp::from(2),
        Kind::Metadata,
        Vec::new(),
        "middle",
    );
    let middle_id = middle.clone().sign_with_keys(&a).unwrap().id;
    let middle_effects = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(middle),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: None,
        },
        Box::new(CapturingReceiptSink::default()),
    ));
    let (middle_receipt, _, _) = find_sign_request(&middle_effects);

    let newest = UnsignedEvent::new(
        a.public_key(),
        Timestamp::from(3),
        Kind::Metadata,
        Vec::new(),
        "newest",
    );
    let newest_id = newest.clone().sign_with_keys(&a).unwrap().id;
    let newest_effects = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(newest),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: None,
        },
        Box::new(CapturingReceiptSink::default()),
    ));
    let (newest_receipt, _, _) = find_sign_request(&newest_effects);

    let older_cancel = core.handle(EngineMsg::CancelWrite(middle_receipt));
    assert!(!all_row_deltas(&older_cancel).iter().any(|delta| {
        matches!(delta, RowDelta::Removed(id) if *id == newest_id)
            || matches!(delta, RowDelta::Added(row) if row.event.id == middle_id)
    }));

    let newest_cancel = core.handle(EngineMsg::CancelWrite(newest_receipt));
    assert!(all_row_deltas(&newest_cancel)
        .iter()
        .any(|delta| matches!(delta, RowDelta::Removed(id) if *id == newest_id)));
    assert!(!all_row_deltas(&newest_cancel)
        .iter()
        .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == middle_id)));
    let fresh = core.handle(EngineMsg::Subscribe(
        literal_query(&[0], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    assert!(all_row_deltas(&fresh).is_empty());
}

#[test]
fn expired_local_acceptance_is_first_and_only_failed_with_no_side_effects() {
    let a = Keys::generate();
    let relay = RelayUrl::parse("wss://write.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay]);
    let mut core = new_core(dir);
    core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    core.handle(EngineMsg::Tick(Timestamp::from(200)));
    let expired = nmp_resolver::testkit::expiring_kind1(&a, "expired", 100, 150);
    let sink = CapturingReceiptSink::default();
    let effects = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Signed(expired),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: None,
        },
        Box::new(sink.clone()),
    ));
    assert!(matches!(
        effects.as_slice(),
        [Effect::EmitReceipt(_, WriteStatus::Failed(_))]
    ));
    assert!(matches!(
        sink.0.lock().unwrap().as_slice(),
        [WriteStatus::Failed(_)]
    ));
}

#[test]
fn exact_duplicate_intents_get_distinct_store_ids_and_one_promotion_advances_both() {
    let a = Keys::generate();
    let relay = RelayUrl::parse("wss://write.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay]);
    let mut core = new_core(dir);
    activate(&mut core, &a);
    let template = unsigned(&a, 1, "same body");

    let first = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(template.clone()),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: None,
        },
        Box::new(CapturingReceiptSink::default()),
    ));
    let (first_id, first_generation, first_template) = find_sign_request(&first);
    let second = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(template),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: None,
        },
        Box::new(CapturingReceiptSink::default()),
    ));
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
        Effect::EmitReceipt(id, WriteStatus::Signed(event_id))
            if *id == first_id && *event_id == signed.id
    )));
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::EmitReceipt(id, WriteStatus::Signed(event_id))
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
    let mut core = new_core(FixtureDirectory::new());
    activate(&mut core, &a);
    connect_signer(&mut core, 0, &ack, a.public_key());
    connect_signer(&mut core, 1, &nack, a.public_key());
    connect_signer(&mut core, 2, &drop_relay, a.public_key());
    authenticate_signer(&mut core, 0, &ack, &a);
    authenticate_signer(&mut core, 1, &nack, &a);
    authenticate_signer(&mut core, 2, &drop_relay, &a);
    let template = unsigned(&a, 1, "same bytes, separate obligations");
    let sink_a = CapturingReceiptSink::default();
    let sink_b = CapturingReceiptSink::default();

    let first = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(template.clone()),
            durability: Durability::Durable,
            routing: WriteRouting::PrivateNarrow(PrivateRoute {
                relays: NarrowOnly::new([ack.clone(), drop_relay.clone()]),
            }),
            identity_override: None,
            correlation: None,
        },
        Box::new(sink_a.clone()),
    ));
    let (id_a, generation_a, to_sign) = find_sign_request(&first);
    let second = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(template),
            durability: Durability::Durable,
            routing: WriteRouting::PrivateNarrow(PrivateRoute {
                relays: NarrowOnly::new([nack.clone()]),
            }),
            identity_override: None,
            correlation: None,
        },
        Box::new(sink_b.clone()),
    ));
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
        Effect::EmitReceipt(id, WriteStatus::Acked(relay)) if *id == id_a && relay == &ack
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
        Effect::EmitReceipt(id, WriteStatus::Rejected(relay, _)) if *id == id_b && relay == &nack
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
        |effect| matches!(effect, Effect::EmitReceipt(id, WriteStatus::GaveUp(_)) if *id == id_a)
    ));
    assert!(
        core.next_deadline().is_some(),
        "durable disconnect arms retry eligibility"
    );
}

#[test]
fn relay_signature_satisfies_all_pending_coowners_and_late_signers_are_ignored() {
    let a = Keys::generate();
    let source = RelayUrl::parse("wss://source.example.com").unwrap();
    let out = RelayUrl::parse("wss://out.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [out.clone()]);
    let mut core = new_core(dir);
    activate(&mut core, &a);
    connect_signer(&mut core, 0, &source, a.public_key());
    connect_signer(&mut core, 1, &out, a.public_key());
    authenticate_signer(&mut core, 0, &source, &a);
    authenticate_signer(&mut core, 1, &out, &a);
    let template = unsigned(&a, 1, "relay wins signing race");
    let sink_a = CapturingReceiptSink::default();
    let sink_b = CapturingReceiptSink::default();
    let first = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(template.clone()),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: None,
        },
        Box::new(sink_a.clone()),
    ));
    let (id_a, generation_a, signer_a) = find_sign_request(&first);
    let second = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(template),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: None,
        },
        Box::new(sink_b.clone()),
    ));
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
            Effect::EmitReceipt(receipt, WriteStatus::Signed(event_id))
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
    let mut core = new_core(FixtureDirectory::new());
    activate(&mut core, &a);
    let sink = CapturingReceiptSink::default();
    let published = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(unsigned(&a, 1, "one operation")),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: None,
        },
        Box::new(sink.clone()),
    ));
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
        Effect::EmitReceipt(rid, WriteStatus::Signed(_)) if *rid == id
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
        let mut core = new_core(FixtureDirectory::new());
        activate(&mut core, &a);
        let sink = CapturingReceiptSink::default();
        let published = core.handle(EngineMsg::Publish(
            WriteIntent {
                payload: WritePayload::Unsigned(unsigned(&a, 1, "survives signer loss")),
                durability: Durability::Durable,
                routing: WriteRouting::AuthorOutbox,
                identity_override: None,
                correlation: None,
            },
            Box::new(sink.clone()),
        ));
        let (id, generation, frozen) = find_sign_request(&published);

        let waiting = core.handle(EngineMsg::SignerCompleted(id, generation, Err(error)));
        assert!(waiting.iter().any(|effect| matches!(
            effect,
            Effect::EmitReceipt(rid, WriteStatus::AwaitingCapability { pubkey })
                if *rid == id && *pubkey == a.public_key()
        )));
        assert!(waiting.iter().any(|effect| matches!(
            effect,
            Effect::RearmSignerIfAvailable(pubkey) if *pubkey == a.public_key()
        )));
        assert_eq!(
            sink.0.lock().unwrap().last(),
            Some(&WriteStatus::AwaitingCapability {
                pubkey: a.public_key()
            })
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
        let mut core = new_core(FixtureDirectory::new());
        activate(&mut core, &a);
        let sink = CapturingReceiptSink::default();
        let published = core.handle(EngineMsg::Publish(
            WriteIntent {
                payload: WritePayload::Unsigned(unsigned(&a, 1, "terminal signer answer")),
                durability: Durability::Durable,
                routing: WriteRouting::AuthorOutbox,
                identity_override: None,
                correlation: None,
            },
            Box::new(sink.clone()),
        ));
        let (id, generation, _) = find_sign_request(&published);

        let failed = core.handle(EngineMsg::SignerCompleted(id, generation, Err(error)));
        assert!(failed.iter().any(|effect| matches!(
            effect,
            Effect::EmitReceipt(rid, WriteStatus::Failed(_)) if *rid == id
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
    let mut core = EngineCore::new(
        FailOnceCompensationStore::new(),
        Box::new(FixtureDirectory::new()),
        10,
    );
    activate(&mut core, &a);
    core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let sink = CapturingReceiptSink::default();
    let published = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(unsigned(&a, 1, "must remain pending")),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: None,
        },
        Box::new(sink.clone()),
    ));
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
    assert_eq!(sink.0.lock().unwrap().as_slice(), [WriteStatus::Accepted]);
    let fresh = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    assert!(all_row_deltas(&fresh)
        .iter()
        .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == event_id)));

    let (outcome, retried) = core.cancel_write(id);
    assert_eq!(
        outcome,
        Ok(nmp_engine::outbox::CancelWriteOutcome::Cancelled)
    );
    assert!(retried.iter().any(
        |effect| matches!(effect, Effect::EmitReceipt(rid, WriteStatus::Cancelled) if *rid == id)
    ));
    assert!(all_row_deltas(&retried)
        .iter()
        .any(|delta| matches!(delta, RowDelta::Removed(removed) if *removed == event_id)));
}

#[test]
fn explicit_cancellation_persistence_failure_keeps_the_obligation_live_until_retry() {
    let a = Keys::generate();
    let mut core = EngineCore::new(
        FailOnceCompensationStore::new(),
        Box::new(FixtureDirectory::new()),
        10,
    );
    activate(&mut core, &a);
    core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let sink = CapturingReceiptSink::default();
    let published = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(unsigned(&a, 2, "cancel must commit first")),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: None,
        },
        Box::new(sink.clone()),
    ));
    let (id, _, template) = find_sign_request(&published);
    let event_id = template.sign_with_keys(&a).unwrap().id;

    let (refused, effects) = core.cancel_write(id);
    assert!(matches!(
        refused,
        Err(nmp_engine::outbox::CancelWriteError::PersistenceFailed {
            receipt_id,
            reason,
        }) if receipt_id == id && reason.contains("injected compensation failure")
    ));
    assert!(
        effects.is_empty(),
        "a refused cancel must emit no terminal fact"
    );
    assert_eq!(sink.0.lock().unwrap().as_slice(), [WriteStatus::Accepted]);

    let fresh = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    assert!(all_row_deltas(&fresh)
        .iter()
        .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == event_id)));

    let (committed, effects) = core.cancel_write(id);
    assert_eq!(
        committed,
        Ok(nmp_engine::outbox::CancelWriteOutcome::Cancelled)
    );
    assert!(effects.iter().any(
        |effect| matches!(effect, Effect::EmitReceipt(rid, WriteStatus::Cancelled) if *rid == id)
    ));
    assert!(all_row_deltas(&effects)
        .iter()
        .any(|delta| matches!(delta, RowDelta::Removed(removed) if *removed == event_id)));
    assert_eq!(
        sink.0.lock().unwrap().as_slice(),
        [WriteStatus::Accepted, WriteStatus::Cancelled]
    );
}

/// #52 Q2 smoking gun: `EngineCore::on_publish` is the ONE place every
/// publish converges (FFI, direct-Rust, `nmp-bdd`'s `EngineThread`), so a
/// `WritePayload::Signed` whose content was tampered with after signing
/// (id/sig stale relative to the new content) must be rejected there,
/// before `WriteStatus::Accepted` is ever emitted and before any
/// `Effect::PublishEvent` is produced -- regardless of caller, with no FFI
/// verify layer anywhere in the loop.
#[test]
fn direct_publish_of_forged_signed_event_is_rejected_before_acceptance() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);
    connect_signer(&mut core, 0, &relay0, a.public_key());

    let genuine = unsigned(&a, 1, "genuine content")
        .sign_with_keys(&a)
        .unwrap();
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

    let sink = CapturingReceiptSink::default();
    let effects = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Signed(forged),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: None,
        },
        Box::new(sink.clone()),
    ));

    assert!(
        matches!(
            effects.as_slice(),
            [Effect::EmitReceipt(_, WriteStatus::Failed(_))]
        ),
        "a forged Signed publish must terminate as the ONLY effect, as Failed -- got {effects:?}"
    );
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::PublishEvent(..))),
        "a forged Signed publish must never produce Effect::PublishEvent"
    );
    let statuses = sink.0.lock().unwrap();
    assert!(
        matches!(statuses.as_slice(), [WriteStatus::Failed(_)]),
        "the sink must see Failed and nothing else -- never Accepted -- got {statuses:?}"
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
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);
    connect_signer(&mut core, 0, &relay0, a.public_key());
    authenticate_signer(&mut core, 0, &relay0, &a);

    let genuine = unsigned(&a, 1, "genuine content")
        .sign_with_keys(&a)
        .unwrap();

    let sink = CapturingReceiptSink::default();
    let effects = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Signed(genuine.clone()),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: None,
        },
        Box::new(sink.clone()),
    ));

    assert!(
        matches!(
            effects.first(),
            Some(Effect::EmitReceipt(_, WriteStatus::Accepted))
        ),
        "a valid Signed publish must still be Accepted first"
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
