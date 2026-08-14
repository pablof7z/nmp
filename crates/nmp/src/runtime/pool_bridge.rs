use std::sync::mpsc::Sender;
use std::time::Duration;

use crossbeam_channel as cb;
use nmp_transport::{PoolEvent, RelayFrame, RelaySessionKey};

use crate::core::{AuthSendCompletion, EngineMsg};

use super::{auth, Cmd};

#[derive(Clone)]
pub(super) struct EnginePoolSink {
    pub(super) events: cb::Sender<PoolEvent>,
    pub(super) stopping: cb::Receiver<()>,
}

impl nmp_transport::PoolEventSink for EnginePoolSink {
    fn on_event(&self, event: PoolEvent) {
        cb::select_biased! {
            recv(self.stopping) -> _ => {}
            send(self.events, event) -> _ => {}
        }
    }
}

/// Blocking translator loop (D8): `PoolEvent` -> `EngineMsg` -> the engine
/// thread's inbox. Exits as soon as `pool_evt_rx` disconnects, which only
/// happens once every clone of the pool's sink is gone (see `EngineThread::
/// join`'s doc).
pub(super) fn pool_bridge_loop(
    pool_evt_rx: &cb::Receiver<PoolEvent>,
    stopping: &cb::Receiver<()>,
    engine_inbox: &Sender<Cmd>,
    max_engine_batch: usize,
    max_engine_batch_bytes: usize,
    max_engine_batch_wait: Duration,
) {
    let mut pending = None;
    loop {
        let event = match pending.take() {
            Some(event) => event,
            None => cb::select_biased! {
                recv(stopping) -> _ => break,
                recv(pool_evt_rx) -> event => match event {
                    Ok(event) => event,
                    Err(_) => break,
                },
            },
        };
        if let PoolEvent::Frame {
            handle,
            session,
            frame,
        } = event
        {
            let Some(first_bytes) = encoded_event_upper_bound(&frame) else {
                if !send_relay_batch(vec![(handle, session, frame)], stopping, engine_inbox) {
                    break;
                }
                continue;
            };
            let mut frames = vec![(handle, session, frame)];
            let mut encoded_bytes = first_bytes;
            let deadline = std::time::Instant::now()
                .checked_add(max_engine_batch_wait)
                .unwrap_or_else(std::time::Instant::now);
            let mut input_closed = false;
            let mut stopped = false;
            loop {
                if frames.len() >= max_engine_batch || encoded_bytes >= max_engine_batch_bytes {
                    break;
                }
                let next = match pool_evt_rx.try_recv() {
                    Ok(event) => Some(event),
                    Err(cb::TryRecvError::Disconnected) => {
                        input_closed = true;
                        None
                    }
                    Err(cb::TryRecvError::Empty) => {
                        let remaining =
                            deadline.saturating_duration_since(std::time::Instant::now());
                        if remaining.is_zero() {
                            None
                        } else {
                            let timeout = cb::after(remaining);
                            cb::select_biased! {
                                recv(stopping) -> _ => {
                                    stopped = true;
                                    None
                                },
                                recv(pool_evt_rx) -> event => match event {
                                    Ok(event) => Some(event),
                                    Err(_) => {
                                        input_closed = true;
                                        None
                                    },
                                },
                                recv(timeout) -> _ => None,
                            }
                        }
                    }
                };
                let Some(next) = next else { break };
                let PoolEvent::Frame {
                    handle,
                    session,
                    frame,
                } = next
                else {
                    pending = Some(next);
                    break;
                };
                let Some(next_bytes) = encoded_event_upper_bound(&frame) else {
                    pending = Some(PoolEvent::Frame {
                        handle,
                        session,
                        frame,
                    });
                    break;
                };
                if encoded_bytes.saturating_add(next_bytes) > max_engine_batch_bytes {
                    pending = Some(PoolEvent::Frame {
                        handle,
                        session,
                        frame,
                    });
                    break;
                }
                encoded_bytes = encoded_bytes.saturating_add(next_bytes);
                frames.push((handle, session, frame));
            }
            if stopped || !send_relay_batch(frames, stopping, engine_inbox) {
                break;
            }
            if input_closed {
                break;
            }
            continue;
        }
        if !forward_pool_event(event, engine_inbox) {
            break; // engine thread is gone; nothing left to feed.
        }
    }
}

fn send_relay_batch(
    frames: Vec<(nmp_transport::RelayHandle, RelaySessionKey, RelayFrame)>,
    stopping: &cb::Receiver<()>,
    engine_inbox: &Sender<Cmd>,
) -> bool {
    let (applied_tx, applied_rx) = cb::bounded(1);
    #[cfg(feature = "bench-instrumentation")]
    {
        let event_bytes = frames
            .iter()
            .filter_map(|(_, _, frame)| encoded_event_upper_bound(frame))
            .fold(0usize, usize::saturating_add);
        crate::ingest_attribution::bridge_batch(frames.len(), event_bytes);
    }
    #[cfg(feature = "bench-instrumentation")]
    let send_started = std::time::Instant::now();
    if engine_inbox
        .send(Cmd::RelayBatch {
            frames,
            applied: applied_tx,
        })
        .is_err()
    {
        return false;
    }
    #[cfg(feature = "bench-instrumentation")]
    crate::ingest_attribution::bridge_send(send_started.elapsed());
    #[cfg(feature = "bench-instrumentation")]
    let applied_started = std::time::Instant::now();
    let applied = cb::select_biased! {
        recv(stopping) -> _ => false,
        recv(applied_rx) -> result => result.is_ok(),
    };
    #[cfg(feature = "bench-instrumentation")]
    crate::ingest_attribution::bridge_applied_wait(applied_started.elapsed());
    applied
}

pub(super) fn encoded_event_upper_bound(frame: &RelayFrame) -> Option<usize> {
    if let RelayFrame::CommittedObservation(hit) = frame {
        return Some(hit.encoded_bytes());
    }
    #[cfg(feature = "bench-instrumentation")]
    if let Some((_, encoded_bytes)) = frame.diagnostic_duplicate_ceiling() {
        return Some(encoded_bytes);
    }
    let event = frame.event()?;
    let tags = event.tags.iter().fold(0usize, |total, tag| {
        tag.as_slice()
            .iter()
            .fold(total.saturating_add(4), |total, atom| {
                total.saturating_add(4).saturating_add(atom.len())
            })
    });
    Some(
        192usize
            .saturating_add(event.content.len())
            .saturating_add(tags),
    )
}

fn forward_pool_event(event: PoolEvent, engine_inbox: &Sender<Cmd>) -> bool {
    match event {
        PoolEvent::WorkerRetired => engine_inbox.send(Cmd::RelayWorkerRetired).is_ok(),
        event => translate_pool_event(event)
            .is_none_or(|message| engine_inbox.send(Cmd::Engine(message)).is_ok()),
    }
}

/// `PoolEvent` -> `EngineMsg` (plan §2/§3.4). Generation safety is already
/// enforced BEFORE this point: `nmp_transport::Pool`'s own translator drops
/// any frame/connect event tagged with a superseded generation before it
/// ever reaches this sink (see `nmp-transport::pool::inner`'s doc) — the
/// `TransportRelayHandle` carried inside `PoolEvent::Connected`/`Frame`
/// already embeds the (verified-current) generation, so forwarding it
/// unchanged into `EngineMsg::RelayConnected`/`RelayFrame` is exactly the
/// "tag frames with the handle's generation" step; there is no further
/// staleness check for this module to perform.
///
pub(super) fn translate_pool_event(event: PoolEvent) -> Option<EngineMsg> {
    match event {
        PoolEvent::Connected { handle, session } => {
            Some(EngineMsg::RelayConnected(handle, session))
        }
        PoolEvent::InitialReadCompleted { handle, session } => {
            Some(EngineMsg::AuthProbeReleased(handle, session))
        }
        // The `reason` is no longer discarded here (issue #506's CRITICAL
        // fix): `EngineCore::on_relay_disconnected` needs to tell a
        // permanent failure (401/403 -- the relay worker has already
        // retired itself, see `nmp_transport::DisconnectReason::
        // PermanentlyFailed`'s doc) apart from an ordinary transient one, so
        // it never re-issues an ensure effect into a busy 401 redial
        // loop.
        PoolEvent::Disconnected {
            handle,
            session,
            reason,
        } => Some(EngineMsg::RelayDisconnected(handle, session, reason)),
        PoolEvent::Frame {
            handle,
            session,
            frame,
        } => Some(EngineMsg::RelayFrame(handle, session, frame)),
        PoolEvent::Health {
            handle,
            session,
            health,
        } => {
            // A worker that is still retrying reports its pre-connect failure
            // as health. Handle-scoped health is deliberately unreachable
            // until RelayConnected establishes this generation, so preserve
            // the failure through the existing session-scoped owner.
            if health.state == nmp_transport::ConnState::Connecting {
                if let Some(reason) = health.last_error.clone() {
                    return Some(EngineMsg::RelayOpenFailed(session, reason));
                }
            }
            Some(EngineMsg::RelayHealth(handle, session, health))
        }
        PoolEvent::EventHandoff {
            correlation,
            result,
        } => Some(EngineMsg::EventHandoff(correlation, result)),
        // Issue #883: the exact ephemeral lane is transport's AUTH send
        // seam. The terminal already names its own exact `(session,
        // handle)` target and opaque operation token, so this translation is
        // total and stateless — no completion map, no callback. The reducer
        // matches it against the send that session is actually awaiting and
        // drops anything else.
        PoolEvent::EphemeralHandoff {
            operation,
            session,
            handle,
            outcome,
        } => Some(EngineMsg::AuthSendCompleted(AuthSendCompletion {
            handle,
            session,
            operation: operation.0,
            outcome: auth::auth_send_outcome(outcome),
        })),
        PoolEvent::WorkerRetired => None,
    }
}
