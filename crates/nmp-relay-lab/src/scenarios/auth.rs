//! NIP-42 READ gating, and what NMP does when a relay will not answer until
//! it knows who is asking.
//!
//! Write gating was the only mode any fixture in this workspace could
//! express, and a relay that gates only writes has no protected READ session
//! at all -- so the class of bug where a client withholds a protected
//! session's REQs pending an AUTH state it never builds could not be
//! reproduced here.

use std::time::Duration;

use nostr::{EventBuilder, Keys, Kind, Tag};

use crate::fixtures::{
    authenticating_engine, engine_against, kind1_by, kind1_by_as, note, notes, rows_within,
    RawSession, QUIET, SETTLE,
};
use crate::scenario::Report;
use crate::{ReadGate, RelayLab, Script};

/// **A finding, captured as a reproduction.** NMP does not complete the
/// read-path NIP-42 handshake, and does not say why.
///
/// The setup is, as far as this crate can determine, complete: a demand with
/// `authenticate_as: Some(key)`, a `LocalKeySigner` account for that exact
/// key -- which does reach the AUTH-capable registry via `registry.add_local`
/// plus `EngineMsg::SignerAttached` -- and an `AuthPolicy` that allows
/// everything. What happens:
///
/// ```text
/// Connecting -> AwaitingAuth{AwaitingChallenge} -> AwaitingAuth{AwaitingPolicy}
///            -> AwaitingAuth{AwaitingSignature} -> AuthDenied
/// ```
///
/// No `["AUTH", <event>]` ever reaches the socket. The policy ALLOWED, so the
/// denial is downstream of it. The path is now traced end to end:
/// `AwaitingSignature -> Denied` has exactly one route,
/// `AuthSignerOutcome::Rejected` (`auth_transport.rs:587`), constructed in
/// exactly one place (`runtime/auth.rs:1007`) out of
/// `SignerError::Rejected(reason)`. The signer refuses to sign the AUTH
/// event, and it says why.
///
/// **The second half stands on its own, whatever the root cause turns out to
/// be: the reason exists and the app never sees it.** It is carried in
/// `SignerError::Rejected(reason)` at the point of refusal and then collapses
/// into a unit `SourceStatus::AuthDenied` before anything an app can read.
/// `AuthDiagnosticsPhase::Denied`'s own doc says it means "the relay rejected
/// the AUTH event, or the policy refused" -- two unrelated causes, one value
/// -- and `AuthDiagnosticsSnapshot` carries `policy_bound`, `signer_bound`
/// and `auth_event_id` but no reason. So an app whose read is blocked here
/// cannot tell a policy denial from a signer rejection from a relay refusal,
/// and neither could this scenario without reading NMP's source.
///
/// This asserts the CURRENT behaviour on purpose. If NMP starts
/// authenticating, this goes red -- and the right response is to rewrite it to
/// assert the handshake completes, not to relax it.
pub async fn read_gate_unanswered(_mutation: Option<&'static str>) -> Report {
    let mut report = Report::new("read-gate-unanswered");
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

    report.that(
        "the relay issued a challenge and refused the REQ with NIP-01's prefix",
        !record.auth_challenges().is_empty()
            && record
                .closed_sent()
                .first()
                .map(|(_, m)| m.starts_with("auth-required:"))
                .unwrap_or(false),
        record.closed_sent(),
    );
    report.that(
        "NMP's policy ALLOWED and a signature was requested",
        phases.iter().any(|p| p.contains("AwaitingSignature")),
        &phases,
    );
    report.that(
        "and the read proceeded rather than being denied",
        phases.iter().any(|p| p.contains("FinishedStoredEvents"))
            && !phases.iter().any(|p| p.contains("AuthDenied")),
        &phases,
    );
    report.that(
        "because the signed AUTH event reached the socket",
        !record.auth_responses().is_empty(),
        record.auth_responses().len(),
    );

    let diagnostics = engine.observe_diagnostics().expect("diagnostics open");
    let snapshot = diagnostics.recv().expect("a snapshot arrives");
    let session = snapshot
        .auth_sessions
        .first()
        .expect("the auth session is visible at all");
    report.that(
        "the phase is Ready: the CLOSED auth-required was read as a demand, not a refusal",
        matches!(session.phase, nmp::AuthDiagnosticsPhase::Ready)
            && session.auth_event_id.is_some(),
        format!("{:?}", session.phase),
    );
    report
}

/// Per-subscriber scoping: events `p`-tagged to one key, two authenticated
/// sessions, and only the involved one is served.
///
/// Driven on raw sockets rather than through NMP: two simultaneous identities
/// against one relay is a property of the RELAY, and proving it through one
/// engine's session would conflate the two. The capability has to be proven
/// regardless: without it an AUTH suite is
/// vacuously green, since a relay that challenges everybody and then serves
/// everybody the same rows proves the handshake completed and nothing else.
pub async fn identity_scoping(mutation: Option<&'static str>) -> Report {
    let mut report = Report::new("identity-scoping");
    let author = Keys::generate();
    let addressee = Keys::generate();
    let stranger = Keys::generate();

    let for_addressee = EventBuilder::new(Kind::TextNote, "addressed to one person")
        .tag(Tag::public_key(addressee.public_key()))
        .custom_created_at(nostr::Timestamp::from_secs(1_700_000_001))
        .sign_with_keys(&author)
        .expect("the fixture note signs");

    let gate = if mutation == Some("drop-scoping") {
        ReadGate::everything()
    } else {
        ReadGate::everything().scoped_to_involved_pubkey()
    };
    let relay = RelayLab::start(
        Script::new()
            .seed([
                note(&author, "involves nobody in particular", 1_700_000_000),
                for_addressee.clone(),
            ])
            .gate_reads(gate),
    )
    .await;

    async fn served_to(port: u16, url: String, keys: Keys) -> Vec<String> {
        let mut session = RawSession::connect(port).await;
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
    report.eq(
        "the addressee is served the note that p-tags them, and nothing else",
        served_to(relay.port(), url.clone(), addressee).await,
        vec![for_addressee.id.to_hex()],
    );
    let to_stranger = served_to(relay.port(), url, stranger).await;
    report.that(
        "an authenticated stranger is served nothing -- this is scoping, not \
         a failed handshake",
        to_stranger.is_empty(),
        &to_stranger,
    );
    report
}

/// Only the gated kinds are gated.
pub async fn ungated_kind(_mutation: Option<&'static str>) -> Report {
    let mut report = Report::new("ungated-kind");
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
    report.eq("kind:1 is not gated here, so it is served", rows.len(), 2);
    report.that(
        "nothing was refused",
        record.closed_sent().is_empty(),
        record.closed_sent(),
    );
    report.that(
        "and no challenge was issued at all",
        record.auth_challenges().is_empty(),
        record.auth_challenges(),
    );
    report
}

/// The fixture validates the BINDING rather than accepting any kind:22242.
///
/// Spoken on a raw socket, because NMP would never send these. A relay that
/// accepts an unbound response makes every scenario above pass without the
/// client binding to anything, so this is the guard on the guard.
pub async fn auth_binding(_mutation: Option<&'static str>) -> Report {
    let mut report = Report::new("auth-binding");
    let relay = RelayLab::start(Script::new().gate_reads(ReadGate::everything())).await;
    let keys = Keys::generate();
    let url = relay.url().to_string();

    let mut session = RawSession::connect(relay.port()).await;
    session.send(r#"["REQ","probe",{"kinds":[1]}]"#).await;
    let refusal = session.read_messages(Duration::from_secs(2)).await;
    let challenge = refusal
        .iter()
        .find_map(|m| {
            let a = m.as_array()?;
            (a.first()?.as_str()? == "AUTH").then(|| a.get(1)?.as_str())?
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
    let refused = |replies: &[serde_json::Value]| {
        replies.iter().any(|m| {
            let a = m.as_array().unwrap();
            a.first().unwrap() == "OK" && a.get(2) == Some(&serde_json::Value::Bool(false))
        })
    };

    session
        .send(&serde_json::json!(["AUTH", signed("not-the-challenge", &url)]).to_string())
        .await;
    let replies = session.read_messages(Duration::from_secs(2)).await;
    report.that(
        "a response bound to a challenge this relay never issued is refused",
        refused(&replies),
        &replies,
    );

    session
        .send(&serde_json::json!(["AUTH", signed(&challenge, "ws://somewhere.else")]).to_string())
        .await;
    let replies = session.read_messages(Duration::from_secs(2)).await;
    report.that(
        "a response naming a different relay is refused",
        refused(&replies),
        &replies,
    );

    session
        .send(&serde_json::json!(["AUTH", signed(&challenge, &url)]).to_string())
        .await;
    let replies = session.read_messages(Duration::from_secs(2)).await;
    report.that(
        "and a correctly bound one is accepted",
        replies.iter().any(|m| {
            let a = m.as_array().unwrap();
            a.first().unwrap() == "OK" && a.get(2) == Some(&serde_json::Value::Bool(true))
        }),
        &replies,
    );
    report
}
