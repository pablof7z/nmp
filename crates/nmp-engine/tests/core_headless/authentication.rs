use super::*;

// ---- protected read authentication -------------------------------------

#[test]
fn fresh_protected_read_ensures_one_worker_and_replays_only_current_demand_after_auth() {
    let signer = Keys::generate();
    let relay = RelayUrl::parse("wss://fresh-protected-read.example").unwrap();
    let session = signer_session(&relay, signer.public_key());
    let mut core = new_core(FixtureRoutingFacts::new());

    let first = core.handle_and_flush(EngineMsg::Subscribe(protected_pinned_query(
        &relay,
        signer.public_key(),
        1,
    )));
    let first_id = subscribed_handle(&first);
    assert_eq!(
        first
            .iter()
            .filter(
                |effect| matches!(effect, Effect::EnsureReadRelay(candidate) if candidate == &session)
            )
            .count(),
        1,
        "fresh protected demand emits one deduplicated worker-acquisition edge"
    );
    assert_req_reaches_the_wire(&first, &session);

    let generation_one = RelayHandle {
        slot: 0,
        generation: 1,
    };
    let connected = core.handle(EngineMsg::RelayConnected(generation_one, session.clone()));
    assert_req_reaches_the_wire(&connected, &session);

    let second = core.handle_and_flush(EngineMsg::Subscribe(protected_pinned_query(
        &relay,
        signer.public_key(),
        2,
    )));
    let second_id = subscribed_handle(&second);
    assert_eq!(
        second
            .iter()
            .filter(
                |effect| matches!(effect, Effect::EnsureReadRelay(candidate) if candidate == &session)
            )
            .count(),
        0,
        "a demand recompile must not reacquire an already connected protected worker"
    );
    assert_req_reaches_the_wire(&second, &session);

    let newest_only = core.handle(EngineMsg::Unsubscribe(first_id));
    assert_eq!(
        newest_only
            .iter()
            .filter(
                |effect| matches!(effect, Effect::EnsureReadRelay(candidate) if candidate == &session)
            )
            .count(),
        0,
        "shrinking the parked plan retains the connected protected worker without reacquiring it"
    );
    // The withdrawal's CLOSE is on the wire for the same reason the REQ was:
    // this session transmitted, so there is something to withdraw.
    assert!(
        newest_only.iter().any(|effect| match effect {
            Effect::Wire(delta) => delta.ops.iter().any(|(candidate, ops)| {
                candidate == &session && ops.iter().any(|op| matches!(op, WireOp::Close(..)))
            }),
            _ => false,
        }),
        "withdrawing one of two live requests must withdraw it on the wire: {newest_only:?}"
    );

    let ready = authenticate_signer(&mut core, 0, &relay, &signer);
    let replay = ready
        .iter()
        .find_map(|effect| match effect {
            Effect::Replay(candidate, reqs) if candidate == &session => Some(reqs),
            _ => None,
        })
        .expect("current AUTH readiness replays the parked current plan");
    assert_eq!(replay.len(), 1);
    assert_eq!(replay[0].filter.kinds, Some(BTreeSet::from([2])));

    let disconnected = core.handle(EngineMsg::RelayDisconnected(
        generation_one,
        session.clone(),
        nmp_transport::DisconnectReason::Error,
    ));
    assert!(disconnected.iter().any(
        |effect| matches!(effect, Effect::EnsureReadRelay(candidate) if candidate == &session)
    ));

    let generation_two = RelayHandle {
        slot: 0,
        generation: 2,
    };
    let reconnected = core.handle(EngineMsg::RelayConnected(generation_two, session.clone()));
    // A fresh generation replays the current plan immediately -- it does not
    // wait to be challenged first (#1889). The relay's challenge is what
    // answers this REQ, not what precedes it.
    assert_req_reaches_the_wire(&reconnected, &session);
    let challenged = core.handle(EngineMsg::RelayFrame(
        generation_two,
        session.clone(),
        RelayFrame::from(RelayMessage::Auth {
            challenge: Cow::Borrowed("fresh-reconnect-challenge"),
        }),
    ));
    assert!(challenged.iter().any(|effect| matches!(
        effect,
        Effect::RelayAuth(AuthEffect::RequestPolicy {
            token,
            challenge,
            ..
        })
            if token.epoch.handle == generation_two
                && token.epoch.session == session
                && challenge == "fresh-reconnect-challenge"
    )));

    let removed = core.handle(EngineMsg::Unsubscribe(second_id));
    assert!(
        !removed.iter().any(
            |effect| matches!(effect, Effect::EnsureReadRelay(candidate) if candidate == &session)
        ),
        "the final demand withdrawal must not reopen the protected session"
    );
}
