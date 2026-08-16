use nmp_grammar::ConcreteFilter;
use nmp_router::{SubId, WireOp};
use nmp_transport::{Pool, RelaySessionKey, WireFrame};
use nostr::{ClientMessage, JsonUtil, SubscriptionId};

use nmp_engine::core::{self, LocalSendRefusal, RequestHandoffOutcome};

/// `Effect::Wire`'s per-session ops -> wire frames. `ensure_session` is
/// idempotent for an already-live slot (ships
/// the frame onto whichever generation is current, queuing it if the socket
/// is still dialing) and transparently reopens a previously-closed one, so
/// there is no separate "is this session already open" bookkeeping to keep
/// here.
///
/// The transport reconnect preamble stays empty for both Public and
/// PROTECTED sessions. Public replay belongs to `EngineCore`; protected REQs
/// additionally cannot replay before the fresh generation reaches AUTH
/// Ready (#8).
pub(super) fn apply_wire_delta(
    delta: &core::AttemptedWireDelta,
    pool: &Pool,
) -> Vec<RequestHandoffOutcome> {
    let mut outcomes = Vec::new();
    for (session, ops) in &delta.ops {
        let has_req = ops.iter().any(|op| matches!(op, WireOp::Req(..)));
        let handle = if has_req {
            pool.ensure_session(session).ok()
        } else {
            // A close-only delta must never reopen a worker already released
            // by exact session-demand reconciliation. Socket teardown already
            // withdrew every subscription on that connection.
            pool.live_session_handle(session)
        };
        for op in ops {
            match op {
                WireOp::Req(sub_id, filter) => {
                    let attempt_id = delta.attempt_id(session, sub_id, filter);
                    let text = req_frame_text(sub_id, filter);
                    outcomes.push(match handle {
                        Some(handle) if pool.send(handle, WireFrame::Text(text)) => {
                            RequestHandoffOutcome::Accepted { attempt_id, handle }
                        }
                        Some(handle) => RequestHandoffOutcome::Refused {
                            attempt_id,
                            cause: LocalSendRefusal::WorkerAdmissionRefused { handle },
                        },
                        None => RequestHandoffOutcome::Refused {
                            attempt_id,
                            cause: LocalSendRefusal::SessionUnavailable,
                        },
                    });
                }
                WireOp::Close(sub_id) => {
                    let text = close_frame_text(sub_id);
                    if let Some(handle) = handle {
                        let _ = pool.send(handle, WireFrame::Text(text));
                    }
                }
            }
        }
        if let Some(handle) = handle {
            pool.set_reconnect_preamble(handle, Vec::new());
        }
    }
    outcomes
}

/// `Effect::Replay`: for a Public session, `reqs` is `EngineCore`'s current
/// plan minus requests already accepted on this exact transport handle. For a
/// protected session it is the full plan released by the AUTH reducer's ready
/// transition (`finish_auth_ok`). Sending these on the exact connected handle
/// is the sole replay owner. No transport preamble is installed, so the same
/// generation cannot receive an automatic copy; protected sessions retain the
/// same empty-preamble rule (#8).
pub(super) fn apply_replay(
    session: &RelaySessionKey,
    reqs: &core::AttemptedReplay,
    pool: &Pool,
) -> Vec<RequestHandoffOutcome> {
    let Ok(handle) = pool.ensure_session(session) else {
        return reqs
            .attempts()
            .iter()
            .copied()
            .map(|attempt_id| RequestHandoffOutcome::Refused {
                attempt_id,
                cause: LocalSendRefusal::SessionUnavailable,
            })
            .collect();
    };
    let mut outcomes = Vec::new();
    for (req, attempt_id) in reqs.iter().zip(reqs.attempts()) {
        let text = req_frame_text(&req.sub_id, &req.filter);
        outcomes.push(if pool.send(handle, WireFrame::Text(text)) {
            RequestHandoffOutcome::Accepted {
                attempt_id: *attempt_id,
                handle,
            }
        } else {
            RequestHandoffOutcome::Refused {
                attempt_id: *attempt_id,
                cause: LocalSendRefusal::WorkerAdmissionRefused { handle },
            }
        });
    }
    pool.set_reconnect_preamble(handle, Vec::new());
    outcomes
}

/// The wire `["REQ", sub_id, filter]` text for `sub_id`/`filter`, using the
/// EXACT same wire subscription-id string `core::attribution` records at
/// send time (`core::wire_sub_id_string`) -- the relay echoes this string
/// back verbatim in EOSE/CLOSED, and `AttributionState::attribute_eose`
/// looks it up by that literal string, so any divergence here would silently
/// break coverage attribution.
fn req_frame_text(sub_id: &SubId, filter: &ConcreteFilter) -> String {
    let wire_id = SubscriptionId::new(core::wire_sub_id_string(sub_id));
    ClientMessage::req(wire_id, vec![filter.to_nostr()]).as_json()
}

/// The wire `["CLOSE", sub_id]` text for `sub_id` (same wire-id convention
/// as [`req_frame_text`]).
pub(super) fn close_frame_text(sub_id: &SubId) -> String {
    let wire_id = SubscriptionId::new(core::wire_sub_id_string(sub_id));
    ClientMessage::close(wire_id).as_json()
}
