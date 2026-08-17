//! #1646 falsifier: a full NIP-42 AUTH transition sequence
//! (challenge -> policy -> signer -> send -> OK) must project fresh
//! evidence for every observation it touches without re-reading the
//! store's canonical rows -- and that cost must not scale with how many
//! observations are live. Before the fix, every one of the 19 AUTH-path
//! call sites in `auth_transport.rs` ran `refresh_all_observations`, which
//! reopens the store's event indexes once per live observation, on EVERY
//! phase transition. After the fix, those sites route through
//! `refresh_all_observation_evidence`/`refresh_all_history_evidence`
//! (`query.rs`/`history_lifecycle.rs`), which only re-derives
//! `AcquisitionEvidence` and never calls `rows_for`/`observation_rows_for`
//! -- the exact functions that increment `projection_store_queries`
//! (`query.rs:3745-3747`).

use super::*;
use nmp_grammar::{Binding, Demand, Filter, IdentityField};
use nostr::Keys;
use std::borrow::Cow;

fn protected_relay() -> RelayUrl {
    RelayUrl::parse("wss://protected.example").unwrap()
}

fn signer_session(relay: &RelayUrl, signer: PublicKey) -> RelaySessionKey {
    RelaySessionKey::new(relay.clone(), AccessContext::Nip42(signer))
}

/// A protected, exact-provider query -- pinned to `relay` under `signer`'s
/// NIP-42 access context, so its ONLY covering source is the exact session
/// this test drives through the AUTH sequence, and its evidence therefore
/// tracks that session's AUTH phase directly.
fn protected_pinned_query(relay: &RelayUrl, signer: PublicKey, authors: Binding) -> LiveQuery {
    LiveQuery::single(
        Demand::new(
            Filter {
                kinds: Some(BTreeSet::from([1u16])),
                authors: Some(authors),
                ..Filter::default()
            },
            SourceAuthority::Pinned(BTreeSet::from([relay.clone()])),
            AccessContext::Nip42(signer),
        )
        .expect("protected pinned demand is valid"),
    )
}

/// Drive one relay generation from `RelayConnected` through a successful
/// `finish_auth_ok`, exactly as `EngineCore` would see a real relay's NIP-42
/// challenge/policy/signer/send/OK round trip. Returns the accumulated
/// effects of the connect step only -- the AUTH sequence itself is driven
/// (and its effects discarded) so the caller can reset the store-query
/// counters at the exact point the falsifier cares about: after connect,
/// before the first AUTH phase transition.
fn connect(core: &mut EngineCore, slot: u32, url: &RelayUrl, signer: PublicKey) -> Vec<Effect> {
    let handle = TransportRelayHandle {
        slot,
        generation: 1,
    };
    let mut effects = core.handle(EngineMsg::RelayConnected(
        handle,
        signer_session(url, signer),
    ));
    effects.extend(core.handle(EngineMsg::RelayInformationResolved(url.clone(), None)));
    effects
}

fn authenticate(core: &mut EngineCore, slot: u32, url: &RelayUrl, signer: &Keys) {
    let handle = TransportRelayHandle {
        slot,
        generation: 1,
    };
    let session = signer_session(url, signer.public_key());

    let challenge = core.handle(EngineMsg::RelayFrame(
        handle,
        session.clone(),
        RelayFrame::from(RelayMessage::Auth {
            challenge: Cow::Owned(format!("falsifier-{slot}")),
        }),
    ));
    let policy_token = challenge
        .into_iter()
        .find_map(|effect| match effect {
            Effect::RelayAuth(AuthEffect::RequestPolicy { token, .. }) => Some(token),
            _ => None,
        })
        .expect("AUTH challenge requests policy for the exact session");

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
        .expect("allowed AUTH policy requests the frozen event signature");

    let signed = unsigned
        .sign_with_keys(signer)
        .expect("sign deterministic AUTH fixture");
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
            Effect::RelayAuth(AuthEffect::Send { token, event }) => Some((token, event)),
            _ => None,
        })
        .expect("signed AUTH requests an exact-generation send");
    core.handle(EngineMsg::AuthSendCompleted(
        AuthSendCompletion::for_operation(&send_token, AuthSendOutcome::Accepted),
    ));
    core.handle(EngineMsg::RelayFrame(
        handle,
        session,
        RelayFrame::from(RelayMessage::ok(auth_event.id, true, "authenticated")),
    ));
}

/// The falsifier from #1646: N observations covered by ONE protected
/// session (plus one bound to `ActivePubkey` on that same session) must
/// see their store-index-reopen count stay flat across a full AUTH
/// sequence, never scale with N. Before the fix this counter was N * (the
/// number of `refresh_all_observations` call sites the sequence crosses) --
/// after it, it is zero: the sequence changes only session/coverage
/// evidence, never a canonical row.
#[test]
fn auth_sequence_reopens_the_store_zero_times_regardless_of_observation_count() {
    let relay = protected_relay();
    let signer = Keys::generate();
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);

    const N: usize = 6;
    for _ in 0..N {
        let author = Keys::generate().public_key().to_hex();
        core.handle(EngineMsg::Subscribe(protected_pinned_query(
            &relay,
            signer.public_key(),
            Binding::Literal(BTreeSet::from([author])),
        )));
    }
    core.handle(EngineMsg::SetActivePubkey(Some(signer.public_key())));
    core.handle(EngineMsg::Subscribe(protected_pinned_query(
        &relay,
        signer.public_key(),
        Binding::Reactive(IdentityField::ActivePubkey),
    )));

    connect(&mut core, 0, &relay, signer.public_key());

    // Reset exactly where the falsifier is scoped: after the connect-time
    // setup settles, before the AUTH sequence itself runs.
    core.projection_store_queries.set(0);
    core.history_store_queries.set(0);

    authenticate(&mut core, 0, &relay, &signer);

    assert_eq!(
        core.projection_store_queries.get(),
        0,
        "a full AUTH sequence changes session/coverage evidence only -- it must never reopen \
         the store's canonical rows for any of the {N} observations riding this session"
    );
    assert_eq!(
        core.history_store_queries.get(),
        0,
        "same rule for history sessions: AUTH transitions carry no canonical-row work"
    );
}

/// #1803: `EngineCore::handle`'s epilogue (`prune_unowned_relay_state`) can
/// append effects after ANY arm returns, including
/// `on_auth_capability_bound`'s -- which returns an empty `Vec` on every one
/// of its own paths. A caller that reads only the arm and concludes "this
/// message never produces effects" is wrong about `handle` as a whole.
///
/// This reproduces the exact precondition the epilogue's third branch
/// checks: a stale `relay_open_failures` entry for a session
/// `auth_required_sessions` also names, that nothing currently requires.
/// Real engine state gets into this shape over several turns (a relay
/// worker failed to open, then every read/write demand on it was withdrawn
/// on a later turn); this test sets it up directly because WHICH turn made
/// it stale is not the point -- what matters is that `AuthCapabilityBound`
/// is the next message the reducer processes while it is stale, so its
/// call is where the cleanup effect surfaces.
#[test]
fn auth_capability_bound_can_surface_an_epilogue_effect_the_arm_itself_never_returns() {
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 8);
    let relay = protected_relay();
    let signer = Keys::generate().public_key();
    let session = signer_session(&relay, signer);

    // Nothing subscribes to or writes through `session`, so
    // `relay_worker_requirements().all` will not contain it -- exactly the
    // "nothing currently requires this session" half of the precondition.
    core.relay_open_failures
        .insert(session.clone(), "injected: relay worker failed to open".into());
    core.auth_required_sessions.insert(session.clone());

    // The token need not resolve to a real in-flight AUTH operation: even
    // when `on_auth_capability_bound` takes its own `_ => return Vec::new()`
    // path, `handle`'s epilogue still runs unconditionally afterward.
    let bogus_token = AuthOpToken {
        epoch: AuthEpoch {
            handle: TransportRelayHandle {
                slot: 0,
                generation: 1,
            },
            session,
            sequence: 0,
        },
        sequence: 0,
    };
    let effects = core.handle(EngineMsg::AuthCapabilityBound {
        token: bogus_token,
        capability: AuthCapability::Policy,
        instance: AuthCapabilityInstance(1),
    });

    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::EmitDiagnostics(_))),
        "handle()'s epilogue prunes the stale relay_open_failures entry and must surface a \
         diagnostics effect on THIS call's return -- the premise the dropped `debug_assert!` at \
         this call site once encoded (\"binding an AUTH capability is a synchronous state-only \
         transition\") is false: {effects:?}"
    );
}
