//! Issue #591 acceptance proof: a caller-generated correlation token
//! survives every crash boundary the issue names, and reattaching by that
//! token behaves identically to the existing by-id door
//! (`reattach_receipt`) once translated. "Restart" is represented the same
//! way `durable_accepted_restart.rs`/`nip46_restart.rs` already establish
//! for this codebase: dropping the whole reducer/store and reopening the
//! real redb file, no sleeps or polling. `nmp-store`'s
//! `redb_store::crash_atomicity_tests` module covers the literal
//! SIGABRT-mid-transaction proof that the `OUTBOX_CORRELATIONS` row commits
//! or rolls back atomically with the receipt it names; this file covers the
//! engine-level replay/reattachment contract across each named boundary.

use std::borrow::Cow;
use std::sync::{Arc, Mutex};

use nmp_engine::core::{
    AuthCapability, AuthCapabilityInstance, AuthEffect, AuthPolicyOutcome, AuthSendOutcome,
    AuthSignerOutcome, Effect, EngineCore, EngineMsg, ReattachOutcome, ReceiptId,
};
use nmp_engine::outbox::{ReceiptSink, WriteStatus};
use nmp_grammar::{
    AccessContext, CorrelationToken, Durability, RelaySessionKey, WriteIntent, WritePayload,
    WriteRouting,
};
use nmp_router::FixtureDirectory;
use nmp_store::{EventStore, RedbStore};
use nmp_transport::{RelayFrame, RelayHandle};
use nostr::{
    EventBuilder, Keys, Kind, PublicKey, RelayMessage, RelayUrl, Timestamp, UnsignedEvent,
};

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<WriteStatus>>>);

impl ReceiptSink for Sink {
    fn on_status(&self, status: WriteStatus) {
        self.0.lock().unwrap().push(status);
    }
}

fn receipt_id(effects: &[Effect]) -> ReceiptId {
    effects
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitReceipt(id, _status) => Some(*id),
            _ => None,
        })
        .expect("every publish emits a receipt id")
}

fn directory(pk: PublicKey, relay: RelayUrl) -> FixtureDirectory {
    FixtureDirectory::new().with_write(pk.to_hex(), [relay])
}

fn token(value: &str) -> CorrelationToken {
    CorrelationToken::new(value).expect("fixture token is within the bounded range")
}

fn unsigned_draft(author: PublicKey, created_at: u64, content: &str) -> UnsignedEvent {
    UnsignedEvent::new(
        author,
        Timestamp::from(created_at),
        Kind::TextNote,
        vec![],
        content,
    )
}

/// Boundary 1: a token that was NEVER accepted -- neither before nor after a
/// real reopen -- resolves `NotFound`, never fabricating an obligation.
#[test]
fn kill_before_acceptance_leaves_the_token_unresolved() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("before-accept.redb");
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://before-accept.example").unwrap();

    {
        let store = RedbStore::open(&path).unwrap();
        let mut core = EngineCore::new(
            store,
            Box::new(directory(keys.public_key(), relay.clone())),
            10,
        );
        let (outcome, resolved_id) =
            core.reattach_by_correlation("never-accepted".to_string(), Box::new(Sink::default()));
        assert_eq!(outcome, ReattachOutcome::NotFound);
        assert_eq!(resolved_id, None);
    }

    // Reopen -- still nothing to find, and reopening itself must not
    // fabricate a mapping.
    let store = RedbStore::open(&path).unwrap();
    let mut core = EngineCore::new(store, Box::new(directory(keys.public_key(), relay)), 10);
    let (outcome, resolved_id) =
        core.reattach_by_correlation("never-accepted".to_string(), Box::new(Sink::default()));
    assert_eq!(outcome, ReattachOutcome::NotFound);
    assert_eq!(resolved_id, None);
}

/// Boundaries 2+3 (collapsed at the store/engine level -- see this file's
/// module doc): the app crashes the INSTANT after a durable accept commits,
/// before it could persist the returned receipt id anywhere. On restart the
/// app has only the token IT minted -- `reattach_by_correlation` recovers
/// the exact same retained obligation a by-id reattach would.
#[test]
fn kill_after_durable_acceptance_reattaches_by_token_alone_after_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("after-accept.redb");
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://after-accept.example").unwrap();
    let tok = "crash-before-persisting-id";

    let original_id = {
        let store = RedbStore::open(&path).unwrap();
        let mut core = EngineCore::new(
            store,
            Box::new(directory(keys.public_key(), relay.clone())),
            10,
        );
        core.handle(EngineMsg::SetActivePubkey(Some(keys.public_key())));
        let effects = core.handle(EngineMsg::Publish(
            WriteIntent {
                payload: WritePayload::Unsigned(unsigned_draft(
                    keys.public_key(),
                    100,
                    "kill-after-accept",
                )),
                durability: Durability::Durable,
                routing: WriteRouting::AuthorOutbox,
                identity_override: None,
                correlation: Some(token(tok)),
            },
            Box::new(Sink::default()),
        ));
        let id = receipt_id(&effects);
        assert!(effects
            .iter()
            .any(|e| matches!(e, Effect::EmitReceipt(r, WriteStatus::Accepted) if *r == id)));
        // The process dies right here -- `id` is known only to this stack
        // frame, never durably recorded by the "app" (this test never
        // writes it to its own storage).
        id
    };

    let store = RedbStore::open(&path).unwrap();
    let mut core = EngineCore::new(store, Box::new(directory(keys.public_key(), relay)), 10);
    core.recover_on_boot();
    let replay = Sink::default();
    let (outcome, resolved_id) =
        core.reattach_by_correlation(tok.to_string(), Box::new(replay.clone()));
    assert_eq!(outcome, ReattachOutcome::Attached);
    assert_eq!(resolved_id, Some(original_id));
    assert_eq!(
        *replay.0.lock().unwrap(),
        vec![
            WriteStatus::Accepted,
            WriteStatus::AwaitingCapability {
                pubkey: keys.public_key()
            },
        ]
    );

    // The by-id door (unreachable to the "app" in this scenario, but usable
    // here to prove the token resolved to the SAME retained obligation, not
    // a distinct one) replays identically.
    let by_id = Sink::default();
    assert_eq!(
        core.reattach_receipt(original_id, Box::new(by_id.clone())),
        ReattachOutcome::Attached
    );
    assert_eq!(*replay.0.lock().unwrap(), *by_id.0.lock().unwrap());
}

/// Boundary 4: a receipt that reached a genuinely TERMINAL state
/// (`Cancelled`) before the crash still resolves by token after restart,
/// replaying the terminal fact rather than losing it or re-opening it.
#[test]
fn terminal_convergence_survives_restart_and_replays_by_token() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("terminal.redb");
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://terminal-correlation.example").unwrap();
    let tok = "terminal-token";

    {
        let store = RedbStore::open(&path).unwrap();
        let mut core = EngineCore::new(
            store,
            Box::new(directory(keys.public_key(), relay.clone())),
            10,
        );
        core.handle(EngineMsg::SetActivePubkey(Some(keys.public_key())));
        let effects = core.handle(EngineMsg::Publish(
            WriteIntent {
                payload: WritePayload::Unsigned(unsigned_draft(
                    keys.public_key(),
                    200,
                    "terminal correlation",
                )),
                durability: Durability::Durable,
                routing: WriteRouting::AuthorOutbox,
                identity_override: None,
                correlation: Some(token(tok)),
            },
            Box::new(Sink::default()),
        ));
        let id = receipt_id(&effects);
        core.handle(EngineMsg::CancelWrite(id));
    }

    let store = RedbStore::open(&path).unwrap();
    let mut core = EngineCore::new(store, Box::new(directory(keys.public_key(), relay)), 10);
    let replay = Sink::default();
    let (outcome, resolved_id) =
        core.reattach_by_correlation(tok.to_string(), Box::new(replay.clone()));
    assert_eq!(outcome, ReattachOutcome::Attached);
    assert!(resolved_id.is_some());
    assert_eq!(*replay.0.lock().unwrap(), vec![WriteStatus::Cancelled]);
}

/// Boundary 5: a caller that does not know whether its first publish
/// landed retries with the SAME token after a restart. This must reattach
/// the existing obligation -- never mint a second intent/receipt, and
/// never let the re-composed (different body/timestamp) second draft
/// become canonical.
#[test]
fn double_submit_same_token_across_a_restart_mints_no_second_obligation() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("double-submit.redb");
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://double-submit.example").unwrap();
    let tok = "idempotent-retry-token";

    let first_id = {
        let store = RedbStore::open(&path).unwrap();
        let mut core = EngineCore::new(
            store,
            Box::new(directory(keys.public_key(), relay.clone())),
            10,
        );
        core.handle(EngineMsg::SetActivePubkey(Some(keys.public_key())));
        let effects = core.handle(EngineMsg::Publish(
            WriteIntent {
                payload: WritePayload::Unsigned(unsigned_draft(
                    keys.public_key(),
                    300,
                    "first body",
                )),
                durability: Durability::Durable,
                routing: WriteRouting::AuthorOutbox,
                identity_override: None,
                correlation: Some(token(tok)),
            },
            Box::new(Sink::default()),
        ));
        receipt_id(&effects)
    };

    // Restart, then retry with a DIFFERENT body/timestamp under the SAME
    // token -- the exact "re-composed draft" scenario the token exists for.
    let store = RedbStore::open(&path).unwrap();
    let mut core = EngineCore::new(store, Box::new(directory(keys.public_key(), relay)), 10);
    core.recover_on_boot();
    core.handle(EngineMsg::SetActivePubkey(Some(keys.public_key())));
    let retry_sink = Sink::default();
    let effects = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(unsigned_draft(
                keys.public_key(),
                301,
                "second, different body",
            )),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: Some(token(tok)),
        },
        Box::new(retry_sink.clone()),
    ));
    let retried_id = receipt_id(&effects);
    assert_eq!(
        retried_id, first_id,
        "the same token must resolve to the SAME receipt id, never a second one"
    );
    assert_eq!(
        *retry_sink.0.lock().unwrap(),
        vec![
            WriteStatus::Accepted,
            WriteStatus::AwaitingCapability {
                pubkey: keys.public_key()
            },
        ],
        "the retry's sink must see the ORIGINAL obligation's replayed facts, \
         never the second draft's own fresh acceptance"
    );

    // The second draft's body never became canonical -- only the first
    // ever entered the store. Drop the reducer first: `RedbStore` allows
    // only one open handle per path per process (see `RedbStore::open`'s
    // registration), same discipline as `nip46_restart.rs`'s `drop(store)`.
    drop(core);
    let first_draft = unsigned_draft(keys.public_key(), 300, "first body");
    let first_frozen_id = nostr::EventId::new(
        &first_draft.pubkey,
        &first_draft.created_at,
        &first_draft.kind,
        &first_draft.tags,
        &first_draft.content,
    );
    let second_draft = unsigned_draft(keys.public_key(), 301, "second, different body");
    let second_frozen_id = nostr::EventId::new(
        &second_draft.pubkey,
        &second_draft.created_at,
        &second_draft.kind,
        &second_draft.tags,
        &second_draft.content,
    );
    let store = RedbStore::open(&path).unwrap();
    assert_eq!(
        store
            .query(&nostr::Filter::new().id(first_frozen_id))
            .expect("query the first draft's id")
            .len(),
        1,
        "the original draft's row exists"
    );
    assert_eq!(
        store
            .query(&nostr::Filter::new().id(second_frozen_id))
            .expect("query the second draft's id")
            .len(),
        0,
        "the re-composed second draft must never become a second canonical row"
    );
}

// -- Boundary 5's sibling, partial relay ACK/reject: mirrors
// `durable_accepted_restart.rs`'s NIP-42 AUTH handshake helper so a SIGNED
// intent can actually reach the wire and receive a real per-relay ACK
// before the simulated crash. Duplicated rather than shared across test
// binaries (each integration test file is its own crate).

fn signer_session(relay: &RelayUrl, signer: PublicKey) -> RelaySessionKey {
    RelaySessionKey::new(relay.clone(), AccessContext::Nip42(signer))
}

fn authenticate(
    core: &mut EngineCore<RedbStore>,
    handle: RelayHandle,
    session: &RelaySessionKey,
    signer: &Keys,
) -> Vec<Effect> {
    let challenge = core.handle(EngineMsg::RelayFrame(
        handle,
        session.clone(),
        RelayFrame::from(RelayMessage::Auth {
            challenge: Cow::Owned(format!("correlation-restart-{}", handle.slot)),
        }),
    ));
    let policy_token = challenge
        .into_iter()
        .find_map(|effect| match effect {
            Effect::RelayAuth(AuthEffect::RequestPolicy { token, .. }) => Some(token),
            _ => None,
        })
        .expect("AUTH challenge requests exact-session policy");

    let policy_instance = AuthCapabilityInstance(1);
    core.handle(EngineMsg::AuthCapabilityBound {
        token: policy_token.clone(),
        capability: AuthCapability::Policy,
        instance: policy_instance,
    });
    let signature = core.handle(EngineMsg::AuthPolicyCompleted(
        policy_token,
        Some(policy_instance),
        AuthPolicyOutcome::Allow,
    ));
    let (sign_token, unsigned) = signature
        .into_iter()
        .find_map(|effect| match effect {
            Effect::RelayAuth(AuthEffect::RequestSignature { token, unsigned }) => {
                Some((token, unsigned))
            }
            _ => None,
        })
        .expect("allowed AUTH policy requests signature");

    let signed = unsigned.sign_with_keys(signer).unwrap();
    let signer_instance = AuthCapabilityInstance(2);
    core.handle(EngineMsg::AuthCapabilityBound {
        token: sign_token.clone(),
        capability: AuthCapability::Signer,
        instance: signer_instance,
    });
    let send = core.handle(EngineMsg::AuthSignerCompleted(
        sign_token,
        Some(signer_instance),
        AuthSignerOutcome::Signed(signed),
    ));
    let (send_token, auth_event) = send
        .into_iter()
        .find_map(|effect| match effect {
            Effect::RelayAuth(AuthEffect::Send { token, event, .. }) => Some((token, event)),
            _ => None,
        })
        .expect("signed AUTH requests exact-generation send");
    core.handle(EngineMsg::AuthSendCompleted(
        send_token,
        AuthSendOutcome::Accepted,
    ));
    core.handle(EngineMsg::RelayFrame(
        handle,
        session.clone(),
        RelayFrame::from(RelayMessage::ok(auth_event.id, true, "authenticated")),
    ))
}

/// Boundary 3: a durable SIGNED intent reaches the wire, one relay ACKs it,
/// then the process crashes before the app could persist the receipt id.
/// After restart, `reattach_by_correlation` recovers the SAME partial
/// per-relay state (the ACK) a by-id reattach would -- the token survives
/// in-flight relay evidence, not only the pre-attempt acceptance fact.
#[test]
fn partial_relay_ack_survives_restart_and_replays_by_token() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("partial-ack.redb");
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://partial-ack.example").unwrap();
    let session = signer_session(&relay, keys.public_key());
    let tok = "partial-ack-token";
    let event = EventBuilder::new(Kind::TextNote, "partial ack correlation")
        .custom_created_at(Timestamp::from(400))
        .sign_with_keys(&keys)
        .unwrap();

    {
        let store = RedbStore::open(&path).unwrap();
        let mut core = EngineCore::new(
            store,
            Box::new(directory(keys.public_key(), relay.clone())),
            10,
        );
        let handle = RelayHandle {
            slot: 0,
            generation: 1,
        };
        core.handle(EngineMsg::RelayConnected(handle, session.clone()));
        authenticate(&mut core, handle, &session, &keys);
        let effects = core.handle(EngineMsg::Publish(
            WriteIntent {
                payload: WritePayload::Signed(event.clone()),
                durability: Durability::Durable,
                routing: WriteRouting::AuthorOutbox,
                identity_override: None,
                correlation: Some(token(tok)),
            },
            Box::new(Sink::default()),
        ));
        assert!(effects.iter().any(|effect| matches!(effect,
            Effect::PublishEvent(r, e, _) if r == &session && e == &event
        )));
        let correlation = effects
            .iter()
            .find_map(|effect| match effect {
                Effect::PublishEvent(r, _, correlation) if r == &session => Some(*correlation),
                _ => None,
            })
            .unwrap();
        core.handle(EngineMsg::EventHandoff(
            correlation,
            nmp_transport::HandoffResult::Written,
        ));
        let acked = core.handle(EngineMsg::RelayFrame(
            handle,
            session.clone(),
            RelayFrame::from(RelayMessage::ok(event.id, true, "")),
        ));
        assert!(acked.iter().any(|effect| matches!(
            effect,
            Effect::EmitReceipt(_, WriteStatus::Acked(acked_relay)) if acked_relay == &relay
        )));
        // The process dies right here, before persisting the receipt id.
    }

    let store = RedbStore::open(&path).unwrap();
    let mut core = EngineCore::new(
        store,
        Box::new(directory(keys.public_key(), relay.clone())),
        10,
    );
    core.recover_on_boot();
    let replay = Sink::default();
    let (outcome, resolved_id) =
        core.reattach_by_correlation(tok.to_string(), Box::new(replay.clone()));
    assert_eq!(outcome, ReattachOutcome::Attached);
    assert!(resolved_id.is_some());
    let statuses = replay.0.lock().unwrap();
    assert!(
        statuses
            .iter()
            .any(|status| matches!(status, WriteStatus::Acked(acked_relay) if acked_relay == &relay)),
        "the token must replay the SAME partial per-relay ACK evidence after restart, got {statuses:?}"
    );
}

/// #591 review (PR #604 finding 4): the partial-ACK boundary above proved
/// only the ACK half by token-replay -- the reject half was covered
/// generically by the by-id door's own tests, but never through the
/// correlation-token door specifically. Same shape as
/// `partial_relay_ack_survives_restart_and_replays_by_token`, but the
/// relay's `OK` frame carries `false` (rejected) instead of `true`.
#[test]
fn partial_relay_reject_survives_restart_and_replays_by_token() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("partial-reject.redb");
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://partial-reject.example").unwrap();
    let session = signer_session(&relay, keys.public_key());
    let tok = "partial-reject-token";
    let event = EventBuilder::new(Kind::TextNote, "partial reject correlation")
        .custom_created_at(Timestamp::from(401))
        .sign_with_keys(&keys)
        .unwrap();

    {
        let store = RedbStore::open(&path).unwrap();
        let mut core = EngineCore::new(
            store,
            Box::new(directory(keys.public_key(), relay.clone())),
            10,
        );
        let handle = RelayHandle {
            slot: 0,
            generation: 1,
        };
        core.handle(EngineMsg::RelayConnected(handle, session.clone()));
        authenticate(&mut core, handle, &session, &keys);
        let effects = core.handle(EngineMsg::Publish(
            WriteIntent {
                payload: WritePayload::Signed(event.clone()),
                durability: Durability::Durable,
                routing: WriteRouting::AuthorOutbox,
                identity_override: None,
                correlation: Some(token(tok)),
            },
            Box::new(Sink::default()),
        ));
        assert!(effects.iter().any(|effect| matches!(effect,
            Effect::PublishEvent(r, e, _) if r == &session && e == &event
        )));
        let correlation = effects
            .iter()
            .find_map(|effect| match effect {
                Effect::PublishEvent(r, _, correlation) if r == &session => Some(*correlation),
                _ => None,
            })
            .unwrap();
        core.handle(EngineMsg::EventHandoff(
            correlation,
            nmp_transport::HandoffResult::Written,
        ));
        let rejected = core.handle(EngineMsg::RelayFrame(
            handle,
            session.clone(),
            RelayFrame::from(RelayMessage::ok(event.id, false, "rate-limited")),
        ));
        assert!(rejected.iter().any(|effect| matches!(
            effect,
            Effect::EmitReceipt(_, WriteStatus::Rejected(rejected_relay, reason))
                if rejected_relay == &relay && reason == "rate-limited"
        )));
        // The process dies right here, before persisting the receipt id.
    }

    let store = RedbStore::open(&path).unwrap();
    let mut core = EngineCore::new(
        store,
        Box::new(directory(keys.public_key(), relay.clone())),
        10,
    );
    core.recover_on_boot();
    let replay = Sink::default();
    let (outcome, resolved_id) =
        core.reattach_by_correlation(tok.to_string(), Box::new(replay.clone()));
    assert_eq!(outcome, ReattachOutcome::Attached);
    assert!(resolved_id.is_some());
    let statuses = replay.0.lock().unwrap();
    assert!(
        statuses.iter().any(|status| matches!(
            status,
            WriteStatus::Rejected(rejected_relay, reason)
                if rejected_relay == &relay && reason == "rate-limited"
        )),
        "the token must replay the SAME partial per-relay REJECT evidence after restart, got {statuses:?}"
    );
}
