//! NIP-42 READ gating, and what NMP does when a relay will not answer a
//! question until it knows who is asking.
//!
//! Write gating was the only mode any fixture in this workspace could
//! express, and a relay that gates only writes has no protected READ session
//! at all -- so the class of bug where a client withholds a protected
//! session's REQs pending an AUTH state it never builds could not be
//! reproduced here. These scenarios are that gap closed.

mod support;

use std::time::Duration;

use nmp_relay_lab::{ReadGate, RelayLab, Script};
use nostr::{EventBuilder, Keys, Kind, Tag};
use support::{
    authenticating_engine, engine_against, kind1_by, kind1_by_as, note, notes, rows_within,
    QUIET, SETTLE,
};

/// **A finding, captured as a reproduction.** NMP does not complete the
/// read-path NIP-42 handshake, and does not say why.
///
/// The setup is, as far as this crate can determine, complete: a demand with
/// `authenticate_as: Some(key)` (the only thing that makes a PROTECTED read
/// session exist), a `LocalKeySigner` account registered for that exact key
/// through `add_private_key_account` -- which does reach the AUTH-capable
/// registry, via `registry.add_local` plus `EngineMsg::SignerAttached` -- and
/// an `AuthPolicy` registered for it that allows everything.
///
/// What happens, read off `Frame::evidence` and confirmed on the wire:
///
/// ```text
/// Connecting
///   -> AwaitingAuth { AwaitingChallenge }   REQ sent
///   -> AwaitingAuth { AwaitingPolicy }      challenge received, policy consulted
///   -> AwaitingAuth { AwaitingSignature }   policy ALLOWED
///   -> AuthDenied                           and then nothing
/// ```
///
/// No `["AUTH", <event>]` ever reaches the socket. The policy allowed, so the
/// denial is downstream of it: `AuthSignerOutcome::Rejected` is the only path
/// from `AwaitingSignature` to `Denied`
/// (`nmp-engine/src/core/auth_transport.rs`).
///
/// **The second half of the finding is that the first half was this hard to
/// establish.** `SourceStatus::AuthDenied` is a unit variant carrying no
/// reason. `AuthDiagnosticsPhase::Denied`'s own doc says it means "the relay
/// rejected the AUTH event, or the policy refused" -- two unrelated causes,
/// one value -- and `AuthDiagnosticsSnapshot` carries `policy_bound`,
/// `signer_bound` and `auth_event_id` but no reason either. An app whose read
/// is blocked here cannot tell a policy denial from a signer rejection from a
/// relay refusal, and neither could this test without reading NMP's source.
///
/// This asserts the CURRENT behaviour on purpose. If NMP starts
/// authenticating, this test fails -- and the right response is to rewrite it
/// to assert the handshake completes, not to relax it.
#[tokio::test(flavor = "multi_thread")]
async fn nmp_does_not_answer_a_read_path_challenge_and_does_not_say_why() {
    let author = Keys::generate();
    let relay = RelayLab::start(
        Script::new()
            .seed(notes(&author, 3, 1_700_000_000))
            .gate_reads(ReadGate::everything()),
    )
    .await;

    let engine = authenticating_engine(&relay, &author);
    let subscription = engine
        .observe(kind1_by_as(&author, &relay, &author), None)
        .expect("the observation opens");

    let mut phases: Vec<String> = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(6);
    while std::time::Instant::now() < deadline {
        let Ok(frame) = subscription.recv_timeout(Duration::from_millis(400)) else {
            continue;
        };
        for evidence in &frame.evidence {
            for source in &evidence.sources {
                let rendered = format!("{:?}", source.status);
                if phases.last() != Some(&rendered) {
                    phases.push(rendered);
                }
            }
        }
    }

    relay.wire().wait_quiet(QUIET, SETTLE).await;
    let record = relay.record();

    assert!(
        !record.auth_challenges().is_empty(),
        "the relay issued a challenge"
    );
    assert!(
        record.closed_sent()[0].1.starts_with("auth-required:"),
        "and refused the REQ with NIP-01's own prefix: {:?}",
        record.closed_sent()
    );
    assert!(
        phases.iter().any(|p| p.contains("AwaitingSignature")),
        "the policy allowed and a signature was requested; phases: {phases:?}"
    );
    assert!(
        phases.iter().any(|p| p.contains("AuthDenied")),
        "and the session was then denied; phases: {phases:?}"
    );
    assert!(
        record.auth_responses().is_empty(),
        "with no AUTH event ever reaching the socket: {:?}",
        record.auth_responses()
    );

    let diagnostics = engine.observe_diagnostics().expect("diagnostics open");
    let snapshot = diagnostics.recv().expect("a snapshot arrives");
    let session = snapshot
        .auth_sessions
        .first()
        .expect("the auth session is visible at all");
    assert!(
        matches!(session.phase, nmp::AuthDiagnosticsPhase::Denied),
        "the phase is Denied and says nothing further: {:?}",
        session.phase
    );
    assert!(
        session.auth_event_id.is_none(),
        "no AUTH event was ever built: {:?}",
        session.auth_event_id
    );
}

/// Per-subscriber scoping: events `p`-tagged to one key, two authenticated
/// sessions, and only the involved one is served.
///
/// Driven on raw sockets rather than through NMP, because the finding above
/// means no `nmp::Engine` can currently authenticate a read session at all.
/// The capability is the relay's and has to be proven regardless: without it
/// an AUTH suite is vacuously green, since a relay that challenges everybody
/// and then serves everybody the same rows proves the handshake completed and
/// nothing else.
#[tokio::test(flavor = "multi_thread")]
async fn an_authenticated_session_is_served_only_what_involves_its_own_key() {
    let author = Keys::generate();
    let addressee = Keys::generate();
    let stranger = Keys::generate();

    let for_addressee = EventBuilder::new(Kind::TextNote, "addressed to one person")
        .tag(Tag::public_key(addressee.public_key()))
        .custom_created_at(nostr::Timestamp::from_secs(1_700_000_001))
        .sign_with_keys(&author)
        .expect("the fixture note signs");

    let relay = RelayLab::start(
        Script::new()
            .seed([
                note(&author, "involves nobody in particular", 1_700_000_000),
                for_addressee.clone(),
            ])
            .gate_reads(ReadGate::everything().scoped_to_involved_pubkey()),
    )
    .await;

    async fn served_to(port: u16, url: String, keys: Keys) -> Vec<String> {
        let mut session = support::RawSession::connect(port).await;
        session.send(r#"["REQ","s",{"kinds":[1]}]"#).await;
        let refusal = session.read_messages(Duration::from_secs(2)).await;
        let challenge = refusal
            .iter()
            .find_map(|m| {
                let a = m.as_array()?;
                (a.first()?.as_str()? == "AUTH").then(|| a.get(1)?.as_str())?
            })
            .expect("a challenge was issued")
            .to_string();

        let auth = EventBuilder::new(Kind::from(22242u16), "")
            .tag(Tag::parse(["challenge".to_string(), challenge]).unwrap())
            .tag(Tag::parse(["relay".to_string(), url]).unwrap())
            .sign_with_keys(&keys)
            .expect("the AUTH event signs");
        session
            .send(&serde_json::json!(["AUTH", auth]).to_string())
            .await;
        let accepted = session.read_messages(Duration::from_secs(2)).await;
        assert!(
            accepted.iter().any(|m| {
                let a = m.as_array().unwrap();
                a.first().unwrap() == "OK" && a.get(2) == Some(&serde_json::Value::Bool(true))
            }),
            "the session authenticated: {accepted:?}"
        );

        session.send(r#"["REQ","s",{"kinds":[1]}]"#).await;
        session
            .read_messages(Duration::from_secs(2))
            .await
            .iter()
            .filter_map(|m| {
                let a = m.as_array()?;
                (a.first()?.as_str()? == "EVENT")
                    .then(|| a.get(2)?.get("id")?.as_str().map(str::to_string))?
            })
            .collect()
    }

    let url = relay.url().to_string();
    assert_eq!(
        served_to(relay.port(), url.clone(), addressee).await,
        vec![for_addressee.id.to_hex()],
        "the addressee is served the note that p-tags them, and nothing else"
    );
    let to_stranger = served_to(relay.port(), url, stranger).await;
    assert!(
        to_stranger.is_empty(),
        "an authenticated stranger is served nothing, because nothing here \
         involves them: {to_stranger:?}"
    );
}

/// Only the gated kinds are gated. A relay that gates kind:4 still answers a
/// kind:1 REQ to an unauthenticated session, and NMP must not be blocked on
/// an AUTH it does not need.
#[tokio::test(flavor = "multi_thread")]
async fn an_ungated_kind_is_served_without_any_authentication() {
    let author = Keys::generate();
    let relay = RelayLab::start(
        Script::new()
            .seed(notes(&author, 2, 1_700_000_000))
            .gate_reads(ReadGate::kinds([4, 1059])),
    )
    .await;

    let engine = engine_against(&relay);
    let subscription = engine
        .observe(kind1_by(&author, &relay), None)
        .expect("the observation opens");
    let rows = rows_within(&subscription, Duration::from_secs(4));

    relay.wire().wait_quiet(QUIET, SETTLE).await;
    let record = relay.record();
    assert_eq!(rows.len(), 2, "kind:1 is not gated here, so it is served");
    assert!(
        record.closed_sent().is_empty(),
        "nothing was refused: {:?}",
        record.closed_sent()
    );
    assert!(
        record.auth_challenges().is_empty(),
        "and no challenge was issued at all: {:?}",
        record.auth_challenges()
    );
}

/// The fixture validates the binding rather than accepting any kind:22242.
///
/// Spoken on a raw socket, because NMP would never send these. A relay that
/// accepts an unbound response makes every scenario above pass without the
/// client binding to anything, so this is the guard on the guard.
#[tokio::test(flavor = "multi_thread")]
async fn an_auth_response_bound_to_the_wrong_challenge_or_relay_is_refused() {
    let relay = RelayLab::start(
        Script::new().gate_reads(ReadGate::everything()),
    )
    .await;
    let keys = Keys::generate();
    let url = relay.url().to_string();

    let mut session = support::RawSession::connect(relay.port()).await;
    session.send(r#"["REQ","probe",{"kinds":[1]}]"#).await;
    let refusal = session.read_messages(Duration::from_secs(2)).await;
    let challenge = refusal
        .iter()
        .find_map(|m| {
            let array = m.as_array()?;
            (array.first()?.as_str()? == "AUTH").then(|| array.get(1)?.as_str())?
        })
        .expect("the relay issued a challenge")
        .to_string();

    let signed = |challenge: &str, relay_tag: &str| {
        EventBuilder::new(Kind::from(22242u16), "")
            .tag(Tag::parse(["challenge".to_string(), challenge.to_string()]).unwrap())
            .tag(Tag::parse(["relay".to_string(), relay_tag.to_string()]).unwrap())
            .sign_with_keys(&keys)
            .expect("the AUTH event signs")
    };

    // Wrong challenge.
    session
        .send(&serde_json::json!(["AUTH", signed("not-the-challenge", &url)]).to_string())
        .await;
    let replies = session.read_messages(Duration::from_secs(2)).await;
    assert!(
        replies.iter().any(|m| {
            let a = m.as_array().unwrap();
            a.first().unwrap() == "OK" && a.get(2) == Some(&serde_json::Value::Bool(false))
        }),
        "a response bound to a challenge this relay never issued must be \
         refused: {replies:?}"
    );

    // Wrong relay.
    session
        .send(&serde_json::json!(["AUTH", signed(&challenge, "ws://somewhere.else")]).to_string())
        .await;
    let replies = session.read_messages(Duration::from_secs(2)).await;
    assert!(
        replies.iter().any(|m| {
            let a = m.as_array().unwrap();
            a.first().unwrap() == "OK" && a.get(2) == Some(&serde_json::Value::Bool(false))
        }),
        "a response naming a different relay must be refused: {replies:?}"
    );

    // Correct binding.
    session
        .send(&serde_json::json!(["AUTH", signed(&challenge, &url)]).to_string())
        .await;
    let replies = session.read_messages(Duration::from_secs(2)).await;
    assert!(
        replies.iter().any(|m| {
            let a = m.as_array().unwrap();
            a.first().unwrap() == "OK" && a.get(2) == Some(&serde_json::Value::Bool(true))
        }),
        "and a correctly bound one is accepted: {replies:?}"
    );
}
