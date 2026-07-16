//! Baseline runtime adapter for reducer-owned NIP-42 effects.
//!
//! No app policy registry exists at this checkpoint, so policy evaluation
//! fails closed immediately with the exact operation token. Synchronous
//! registered signers are usable; pending signers are cancelled by drop and
//! reported unavailable until the bounded max-one-operation registry lands
//! in the runtime lane. AUTH handoff is exact-generation, nonpersistent, and
//! never enters reconnect preambles, durable send, or write correlations.

use std::sync::mpsc::Sender;

use nmp_signer::{SignerError, SignerOp};
use nmp_transport::{EphemeralSendOutcome, EphemeralSendStart, Pool, WireFrame};
use nostr::{ClientMessage, JsonUtil};

use crate::core::{AuthEffect, AuthPolicyOutcome, AuthSendOutcome, AuthSignerOutcome, EngineMsg};

use super::{Cmd, SignerRegistry};

pub(super) fn dispatch(
    effect: AuthEffect,
    pool: &Pool,
    registry: &SignerRegistry,
    inbox: &Sender<Cmd>,
) {
    match effect {
        AuthEffect::Cancel(_) => {
            // The baseline adapter never retains an asynchronous operation:
            // policy is unavailable and pending signer values are cancelled
            // immediately by drop. Lane B extends this arm with exact-token
            // task cancellation when its bounded registry lands.
        }
        AuthEffect::RequestPolicy { token, .. } => {
            let _ = inbox.send(Cmd::Engine(EngineMsg::AuthPolicyCompleted(
                token,
                None,
                AuthPolicyOutcome::Unavailable,
            )));
        }
        AuthEffect::RequestSignature { token, unsigned } => {
            let (instance, outcome) = match registry.auth_signer_for(unsigned.pubkey) {
                None => (None, AuthSignerOutcome::Unavailable),
                Some((instance, signer)) => match signer.sign(*unsigned) {
                    SignerOp::Ready(result) => {
                        let outcome = match result {
                            Ok(event) => AuthSignerOutcome::Signed(event),
                            Err(SignerError::Rejected(reason)) => {
                                AuthSignerOutcome::Rejected { reason }
                            }
                            Err(SignerError::InvalidResponse(reason)) => {
                                AuthSignerOutcome::Error { reason }
                            }
                            Err(_) => AuthSignerOutcome::Unavailable,
                        };
                        (Some(instance), outcome)
                    }
                    SignerOp::Pending(pending) => {
                        drop(pending);
                        (Some(instance), AuthSignerOutcome::Unavailable)
                    }
                },
            };
            let _ = inbox.send(Cmd::Engine(EngineMsg::AuthSignerCompleted(
                token, instance, outcome,
            )));
        }
        AuthEffect::Send {
            token,
            epoch,
            event,
        } => {
            let completion_inbox = inbox.clone();
            let completion_token = token.clone();
            let start = pool.send_ephemeral_exact(
                &epoch.session,
                epoch.handle,
                WireFrame::Text(ClientMessage::auth(*event).as_json()),
                move |outcome| {
                    let _ = completion_inbox.send(Cmd::Engine(EngineMsg::AuthSendCompleted(
                        completion_token,
                        auth_send_outcome(outcome),
                    )));
                },
            );
            if let EphemeralSendStart::Resolved(outcome) = start {
                let _ = inbox.send(Cmd::Engine(EngineMsg::AuthSendCompleted(
                    token,
                    auth_send_outcome(outcome),
                )));
            }
        }
    }
}

fn auth_send_outcome(outcome: EphemeralSendOutcome) -> AuthSendOutcome {
    match outcome {
        EphemeralSendOutcome::Accepted => AuthSendOutcome::Accepted,
        EphemeralSendOutcome::Unavailable => AuthSendOutcome::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{AuthEpoch, AuthOpToken};
    use nmp_grammar::{AccessContext, RelaySessionKey};
    use nmp_transport::{PoolConfig, PoolEvent, RelayHandle};
    use nostr::{Keys, RelayUrl};
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    fn token() -> AuthOpToken {
        let relay = RelayUrl::parse("wss://auth-adapter.example.com").unwrap();
        let keys = Keys::generate();
        AuthOpToken {
            epoch: AuthEpoch {
                handle: RelayHandle {
                    slot: 3,
                    generation: 7,
                },
                session: RelaySessionKey::new(relay, AccessContext::Nip42(keys.public_key())),
                sequence: 11,
            },
            sequence: 12,
        }
    }

    #[test]
    fn absent_policy_and_signer_fail_closed_with_the_exact_token() {
        let (pool_tx, _pool_rx) = std::sync::mpsc::channel();
        let pool = Pool::new(PoolConfig::default(), pool_tx).unwrap();
        let registry = SignerRegistry::default();
        let (inbox, rx) = std::sync::mpsc::channel();
        let policy_token = token();
        dispatch(
            AuthEffect::RequestPolicy {
                token: policy_token.clone(),
                expected_pubkey: match policy_token.epoch.session.access {
                    AccessContext::Nip42(pubkey) => pubkey,
                    AccessContext::Public => unreachable!(),
                },
                challenge: "challenge".to_string(),
            },
            &pool,
            &registry,
            &inbox,
        );
        assert!(matches!(
            rx.recv().unwrap(),
            Cmd::Engine(EngineMsg::AuthPolicyCompleted(
                current,
                None,
                AuthPolicyOutcome::Unavailable
            )) if current == policy_token
        ));

        let sign_token = token();
        let unsigned =
            nostr::EventBuilder::auth("challenge", sign_token.epoch.session.relay.clone()).build(
                match sign_token.epoch.session.access {
                    AccessContext::Nip42(pubkey) => pubkey,
                    AccessContext::Public => unreachable!(),
                },
            );
        dispatch(
            AuthEffect::RequestSignature {
                token: sign_token.clone(),
                unsigned: Box::new(unsigned),
            },
            &pool,
            &registry,
            &inbox,
        );
        assert!(matches!(
            rx.recv().unwrap(),
            Cmd::Engine(EngineMsg::AuthSignerCompleted(
                current,
                None,
                AuthSignerOutcome::Unavailable
            )) if current == sign_token
        ));

        let send_token = token();
        let send_keys = Keys::generate();
        let event = nostr::EventBuilder::auth("challenge", send_token.epoch.session.relay.clone())
            .sign_with_keys(&send_keys)
            .unwrap();
        dispatch(
            AuthEffect::Send {
                token: send_token.clone(),
                epoch: send_token.epoch.clone(),
                event: Box::new(event),
            },
            &pool,
            &registry,
            &inbox,
        );
        assert!(matches!(
            rx.recv().unwrap(),
            Cmd::Engine(EngineMsg::AuthSendCompleted(
                current,
                AuthSendOutcome::Unavailable
            )) if current == send_token
        ));
        pool.shutdown();
    }

    #[test]
    fn exact_connected_auth_flush_reports_the_exact_token_accepted() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let (wire_tx, wire_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            let message = socket.read().unwrap();
            let _ = wire_tx.send(message.into_text().unwrap().to_string());
        });

        let relay = RelayUrl::parse(&format!("ws://{address}")).unwrap();
        let keys = Keys::generate();
        let session = RelaySessionKey::new(relay.clone(), AccessContext::Nip42(keys.public_key()));
        let (pool_tx, pool_rx) = std::sync::mpsc::channel();
        // Issue #519/#524 resolved-IP admission refuses a 127.0.0.1 dial
        // unless the operator opts that host in — the same opt-in the
        // transport's own real-socket tests use (`test_pool_config`). This
        // postdates the mega base this reducer was ported from.
        let pool = Pool::new(
            PoolConfig {
                allowed_local_hosts: std::sync::Arc::new(std::collections::BTreeSet::from([
                    "127.0.0.1".to_string(),
                ])),
                ..PoolConfig::default()
            },
            pool_tx,
        )
        .unwrap();
        let opened = pool.ensure_session(&session).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let connected = loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let event = pool_rx.recv_timeout(remaining).unwrap();
            if let PoolEvent::Connected {
                handle,
                session: observed,
            } = event
            {
                assert_eq!(observed, session);
                break handle;
            }
        };
        assert_eq!(connected, opened);

        let token = AuthOpToken {
            epoch: AuthEpoch {
                handle: connected,
                session: session.clone(),
                sequence: 41,
            },
            sequence: 42,
        };
        let event = nostr::EventBuilder::auth("exact-challenge", relay)
            .sign_with_keys(&keys)
            .unwrap();
        let registry = SignerRegistry::default();
        let (inbox, rx) = std::sync::mpsc::channel();
        dispatch(
            AuthEffect::Send {
                token: token.clone(),
                epoch: token.epoch.clone(),
                event: Box::new(event),
            },
            &pool,
            &registry,
            &inbox,
        );

        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            Cmd::Engine(EngineMsg::AuthSendCompleted(
                current,
                AuthSendOutcome::Accepted
            )) if current == token
        ));
        let wire = wire_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(wire.starts_with("[\"AUTH\","));
        assert!(wire.contains("exact-challenge"));

        pool.shutdown();
        server.join().unwrap();
    }
}
