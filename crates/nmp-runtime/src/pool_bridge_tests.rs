use super::*;
use nmp_transport::{ConnState, PoolEventSink, RelayFrame, RelayHandle, RelayHealth};
use nostr::{EventBuilder, Keys, RelayMessage, SubscriptionId};

fn notice_frame(text: &str) -> RelayFrame {
    RelayFrame::from_message(RelayMessage::notice(text))
}

fn event_frame(text: &str) -> RelayFrame {
    let event = EventBuilder::text_note(text)
        .sign_with_keys(&Keys::generate())
        .unwrap();
    RelayFrame::from_message(RelayMessage::event(SubscriptionId::new("sub"), event))
}

fn test_session() -> RelaySessionKey {
    RelaySessionKey::public(RelayUrl::parse("wss://relay.example.com").unwrap())
}

fn protected_session() -> RelaySessionKey {
    RelaySessionKey::new(
        RelayUrl::parse("wss://relay.example.com").unwrap(),
        nmp_grammar::AccessContext::Nip42(nostr::Keys::generate().public_key()),
    )
}

#[test]
fn never_connected_health_becomes_session_scoped_open_failure() {
    let handle = RelayHandle {
        slot: 1,
        generation: 2,
    };
    let session = test_session();
    let message = translate_pool_event(PoolEvent::Health {
        handle,
        session: session.clone(),
        health: RelayHealth {
            state: ConnState::Connecting,
            last_error: Some("connection refused".to_string()),
            ..RelayHealth::default()
        },
    });

    assert!(matches!(
        message,
        Some(EngineMsg::RelayOpenFailed(current, reason))
            if current == session && reason == "connection refused"
    ));
}

#[test]
fn connected_health_remains_generation_scoped() {
    let handle = RelayHandle {
        slot: 1,
        generation: 2,
    };
    let session = test_session();
    let health = RelayHealth {
        state: ConnState::Connected,
        last_error: Some("invalid frame".to_string()),
        invalid_signature_count: 1,
        ..RelayHealth::default()
    };
    let message = translate_pool_event(PoolEvent::Health {
        handle,
        session: session.clone(),
        health: health.clone(),
    });

    assert!(matches!(
        message,
        Some(EngineMsg::RelayHealth(current, current_session, current_health))
            if current == handle
                && current_session == session
                && current_health == health
    ));
}

#[test]
fn buffered_auth_batch_is_applied_before_initial_read_release() {
    let (pool_tx, pool_rx) = cb::bounded(8);
    let (stop_tx, stop_rx) = cb::bounded(0);
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let handle = RelayHandle {
        slot: 1,
        generation: 2,
    };
    let session = protected_session();
    pool_tx
        .send(PoolEvent::Connected {
            handle,
            session: session.clone(),
        })
        .unwrap();
    pool_tx
        .send(PoolEvent::Frame {
            handle,
            session: session.clone(),
            frame: RelayFrame::from_message(RelayMessage::Auth {
                challenge: "bridge-ordered".into(),
            }),
        })
        .unwrap();
    pool_tx
        .send(PoolEvent::InitialReadCompleted {
            handle,
            session: session.clone(),
        })
        .unwrap();
    let bridge = thread::spawn(move || {
        pool_bridge_loop(&pool_rx, &stop_rx, &cmd_tx, 128, usize::MAX, Duration::ZERO)
    });

    assert!(matches!(
        cmd_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        Cmd::Engine(EngineMsg::RelayConnected(current, ref current_session))
            if current == handle && *current_session == session
    ));
    let applied = match cmd_rx.recv_timeout(Duration::from_secs(1)).unwrap() {
        Cmd::RelayBatch { frames, applied } => {
            assert_eq!(frames.len(), 1);
            assert!(relay_frame_is_auth(&frames[0].2));
            applied
        }
        _ => panic!("AUTH must enter the reducer as a relay batch"),
    };
    assert!(
        matches!(cmd_rx.try_recv(), Err(mpsc::TryRecvError::Empty)),
        "the completion edge cannot overtake the stalled AUTH reduction"
    );
    applied.send(()).unwrap();
    assert!(matches!(
        cmd_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        Cmd::Engine(EngineMsg::AuthProbeReleased(current, ref current_session))
            if current == handle && *current_session == session
    ));

    drop(pool_tx);
    drop(stop_tx);
    bridge.join().unwrap();
}

#[test]
fn bridge_waits_for_applied_ack_before_enqueuing_another_relay_batch() {
    let (pool_tx, pool_rx) = cb::bounded(8);
    let (stop_tx, stop_rx) = cb::bounded(0);
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let bridge = thread::spawn(move || {
        pool_bridge_loop(&pool_rx, &stop_rx, &cmd_tx, 128, usize::MAX, Duration::ZERO)
    });
    let handle = RelayHandle {
        slot: 1,
        generation: 2,
    };

    pool_tx
        .send(PoolEvent::Frame {
            handle,
            session: test_session(),
            frame: notice_frame("first"),
        })
        .unwrap();
    let first_ack = match cmd_rx.recv_timeout(Duration::from_secs(1)).unwrap() {
        Cmd::RelayBatch { frames, applied } => {
            assert_eq!(frames.len(), 1);
            applied
        }
        _ => panic!("bridge must emit a relay batch"),
    };

    pool_tx
        .send(PoolEvent::Frame {
            handle,
            session: test_session(),
            frame: notice_frame("second"),
        })
        .unwrap();
    assert!(
        matches!(cmd_rx.try_recv(), Err(mpsc::TryRecvError::Empty)),
        "a second relay batch cannot enter the engine inbox before ack"
    );

    first_ack.send(()).unwrap();
    let second_ack = match cmd_rx.recv_timeout(Duration::from_secs(1)).unwrap() {
        Cmd::RelayBatch { frames, applied } => {
            assert_eq!(frames.len(), 1);
            applied
        }
        _ => panic!("bridge must emit the second relay batch after ack"),
    };
    second_ack.send(()).unwrap();
    drop(pool_tx);
    drop(stop_tx);
    bridge.join().unwrap();
}

#[test]
fn bridge_caps_each_engine_transaction_without_losing_order() {
    let (pool_tx, pool_rx) = cb::bounded(8);
    let (stop_tx, stop_rx) = cb::bounded(0);
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let handle = RelayHandle {
        slot: 1,
        generation: 2,
    };
    for text in ["one", "two", "three"] {
        pool_tx
            .send(PoolEvent::Frame {
                handle,
                session: test_session(),
                frame: event_frame(text),
            })
            .unwrap();
    }
    let bridge = thread::spawn(move || {
        pool_bridge_loop(&pool_rx, &stop_rx, &cmd_tx, 2, usize::MAX, Duration::ZERO)
    });

    let first_ack = match cmd_rx.recv_timeout(Duration::from_secs(1)).unwrap() {
        Cmd::RelayBatch { frames, applied } => {
            assert_eq!(frames.len(), 2);
            assert_eq!(frames[0].2.event().unwrap().content, "one");
            assert_eq!(frames[1].2.event().unwrap().content, "two");
            applied
        }
        _ => panic!("first command must be a capped relay batch"),
    };
    first_ack.send(()).unwrap();
    let second_ack = match cmd_rx.recv_timeout(Duration::from_secs(1)).unwrap() {
        Cmd::RelayBatch { frames, applied } => {
            assert_eq!(frames.len(), 1);
            assert_eq!(frames[0].2.event().unwrap().content, "three");
            applied
        }
        _ => panic!("second command must retain the next ordered frame"),
    };
    second_ack.send(()).unwrap();
    drop(pool_tx);
    drop(stop_tx);
    bridge.join().unwrap();
}

#[test]
fn control_frame_is_a_commit_barrier_between_event_batches() {
    let (pool_tx, pool_rx) = cb::bounded(8);
    let (stop_tx, stop_rx) = cb::bounded(0);
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let handle = RelayHandle {
        slot: 1,
        generation: 2,
    };
    for frame in [
        event_frame("before"),
        notice_frame("barrier"),
        event_frame("after"),
    ] {
        pool_tx
            .send(PoolEvent::Frame {
                handle,
                session: test_session(),
                frame,
            })
            .unwrap();
    }
    let bridge = thread::spawn(move || {
        pool_bridge_loop(&pool_rx, &stop_rx, &cmd_tx, 8, usize::MAX, Duration::ZERO)
    });

    let before_ack = match cmd_rx.recv_timeout(Duration::from_secs(1)).unwrap() {
        Cmd::RelayBatch { frames, applied } => {
            assert_eq!(frames.len(), 1);
            assert_eq!(frames[0].2.event().unwrap().content, "before");
            applied
        }
        _ => panic!("event before barrier must commit first"),
    };
    assert!(matches!(cmd_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
    before_ack.send(()).unwrap();

    let barrier_ack = match cmd_rx.recv_timeout(Duration::from_secs(1)).unwrap() {
        Cmd::RelayBatch { frames, applied } => {
            assert_eq!(frames.len(), 1);
            assert_eq!(
                frames[0].2.clone().into_message(),
                RelayMessage::notice("barrier")
            );
            applied
        }
        _ => panic!("control barrier must be applied after prior commit"),
    };
    assert!(matches!(cmd_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
    barrier_ack.send(()).unwrap();

    let after_ack = match cmd_rx.recv_timeout(Duration::from_secs(1)).unwrap() {
        Cmd::RelayBatch { frames, applied } => {
            assert_eq!(frames.len(), 1);
            assert_eq!(frames[0].2.event().unwrap().content, "after");
            applied
        }
        _ => panic!("event after barrier must remain ordered"),
    };
    after_ack.send(()).unwrap();
    drop(pool_tx);
    drop(stop_tx);
    bridge.join().unwrap();
}

#[test]
fn lifecycle_event_is_a_commit_barrier_between_event_batches() {
    let (pool_tx, pool_rx) = cb::bounded(8);
    let (stop_tx, stop_rx) = cb::bounded(0);
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let handle = RelayHandle {
        slot: 1,
        generation: 2,
    };
    pool_tx
        .send(PoolEvent::Frame {
            handle,
            session: test_session(),
            frame: event_frame("before"),
        })
        .unwrap();
    pool_tx.send(PoolEvent::WorkerRetired).unwrap();
    pool_tx
        .send(PoolEvent::Frame {
            handle,
            session: test_session(),
            frame: event_frame("after"),
        })
        .unwrap();
    let bridge = thread::spawn(move || {
        pool_bridge_loop(&pool_rx, &stop_rx, &cmd_tx, 8, usize::MAX, Duration::ZERO)
    });

    let before_ack = match cmd_rx.recv_timeout(Duration::from_secs(1)).unwrap() {
        Cmd::RelayBatch { frames, applied } => {
            assert_eq!(frames.len(), 1);
            assert_eq!(frames[0].2.event().unwrap().content, "before");
            applied
        }
        _ => panic!("event before lifecycle barrier must commit first"),
    };
    assert!(matches!(cmd_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
    before_ack.send(()).unwrap();

    assert!(matches!(
        cmd_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        Cmd::RelayWorkerRetired
    ));
    let after_ack = match cmd_rx.recv_timeout(Duration::from_secs(1)).unwrap() {
        Cmd::RelayBatch { frames, applied } => {
            assert_eq!(frames.len(), 1);
            assert_eq!(frames[0].2.event().unwrap().content, "after");
            applied
        }
        _ => panic!("event after lifecycle barrier must remain ordered"),
    };
    after_ack.send(()).unwrap();
    drop(pool_tx);
    drop(stop_tx);
    bridge.join().unwrap();
}

#[test]
fn encoded_byte_bound_splits_consecutive_events_without_loss() {
    let first = event_frame(&"a".repeat(512));
    let second = event_frame(&"b".repeat(512));
    let one_event_bytes = encoded_event_upper_bound(&first).unwrap();
    let (pool_tx, pool_rx) = cb::bounded(4);
    let (stop_tx, stop_rx) = cb::bounded(0);
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let handle = RelayHandle {
        slot: 1,
        generation: 2,
    };
    for frame in [first, second] {
        pool_tx
            .send(PoolEvent::Frame {
                handle,
                session: test_session(),
                frame,
            })
            .unwrap();
    }
    let bridge = thread::spawn(move || {
        pool_bridge_loop(
            &pool_rx,
            &stop_rx,
            &cmd_tx,
            8,
            one_event_bytes + 1,
            Duration::ZERO,
        )
    });
    for expected in ['a', 'b'] {
        let ack = match cmd_rx.recv_timeout(Duration::from_secs(1)).unwrap() {
            Cmd::RelayBatch { frames, applied } => {
                assert_eq!(frames.len(), 1);
                assert!(frames[0].2.event().unwrap().content.starts_with(expected));
                applied
            }
            _ => panic!("byte bound must preserve each event"),
        };
        ack.send(()).unwrap();
    }
    drop(pool_tx);
    drop(stop_tx);
    bridge.join().unwrap();
}

#[test]
fn bounded_wait_coalesces_a_short_event_burst() {
    let (pool_tx, pool_rx) = cb::bounded(4);
    let (stop_tx, stop_rx) = cb::bounded(0);
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let handle = RelayHandle {
        slot: 1,
        generation: 2,
    };
    let bridge = thread::spawn(move || {
        pool_bridge_loop(
            &pool_rx,
            &stop_rx,
            &cmd_tx,
            8,
            usize::MAX,
            Duration::from_millis(50),
        )
    });
    pool_tx
        .send(PoolEvent::Frame {
            handle,
            session: test_session(),
            frame: event_frame("first"),
        })
        .unwrap();
    thread::sleep(Duration::from_millis(5));
    pool_tx
        .send(PoolEvent::Frame {
            handle,
            session: test_session(),
            frame: event_frame("second"),
        })
        .unwrap();
    let ack = match cmd_rx.recv_timeout(Duration::from_secs(1)).unwrap() {
        Cmd::RelayBatch { frames, applied } => {
            assert_eq!(frames.len(), 2);
            applied
        }
        _ => panic!("short burst must coalesce"),
    };
    ack.send(()).unwrap();
    drop(pool_tx);
    drop(stop_tx);
    bridge.join().unwrap();
}

#[test]
fn stop_disconnect_releases_bridge_waiting_for_engine_ack() {
    let (pool_tx, pool_rx) = cb::bounded(1);
    let (stop_tx, stop_rx) = cb::bounded(0);
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let bridge = thread::spawn(move || {
        pool_bridge_loop(&pool_rx, &stop_rx, &cmd_tx, 1, usize::MAX, Duration::ZERO)
    });
    pool_tx
        .send(PoolEvent::Frame {
            handle: RelayHandle {
                slot: 1,
                generation: 2,
            },
            session: test_session(),
            frame: notice_frame("pending"),
        })
        .unwrap();
    let _unacked = cmd_rx.recv_timeout(Duration::from_secs(1)).unwrap();

    drop(stop_tx);
    bridge.join().unwrap();
    drop(pool_tx);
}

#[test]
fn bounded_pool_sink_is_cancelled_without_polling_during_shutdown() {
    let (events_tx, events_rx) = cb::bounded(1);
    let (stop_tx, stop_rx) = cb::bounded(0);
    let sink = EnginePoolSink {
        events: events_tx,
        stopping: stop_rx,
    };
    sink.on_event(PoolEvent::Disconnected {
        handle: RelayHandle {
            slot: 1,
            generation: 1,
        },
        session: test_session(),
        reason: nmp_transport::DisconnectReason::Error,
    });
    let blocked = thread::spawn(move || {
        sink.on_event(PoolEvent::Disconnected {
            handle: RelayHandle {
                slot: 2,
                generation: 1,
            },
            session: test_session(),
            reason: nmp_transport::DisconnectReason::Error,
        });
    });

    drop(stop_tx);
    blocked.join().unwrap();
    assert_eq!(events_rx.len(), 1, "shutdown does not enqueue a tail");
}
