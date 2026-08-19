//! Reading from a relay that gates reads behind NIP-42.
//!
//! ## The claim under test
//!
//! "An app that pins `Demand::authenticate_as`, registers a signer for that
//! key and installs an allowing `AuthPolicy` can read from a relay that
//! requires authentication."
//!
//! ## What is actually observed
//!
//! [`GatedRelay`](crate::gated_relay::GatedRelay) does what `strfry` does: it
//! challenges on connect, and answers a `REQ` from an unauthenticated client
//! with `CLOSED <sub> "auth-required: ..."`. It records every client frame, so
//! "no `["AUTH", <event>]` reached the socket" is a measurement here and not
//! an inference.
//!
//! The two facts this scenario prints are the whole finding:
//!
//! - whether an `["AUTH", <event>]` frame ever reached the relay, and
//! - the terminal per-relay [`nmp::SourceStatus`] the app can see.
//!
//! An app that can see `AuthDenied` and nothing else cannot tell "my policy
//! refused", "my signer refused" and "the relay refused" apart, which is why
//! [`Observed::denial`] reads the reason out of the diagnostics snapshot.

use std::time::{Duration, Instant};

use crate::gated_relay::Challenge;
use nmp::{
    AuthDiagnosticsPhase, AuthPolicy, AuthPolicyOp, AuthPolicyRequest, Demand, Engine,
    EngineConfig, Filter, LiveQuery, PublicKey, ReadRouting, RelayUrl, SourceStatus,
};

/// What the app's own policy does.
///
/// `Allow(Duration::ZERO)` is the non-event case, where anything observed
/// downstream is the engine's own. A non-zero delay is the ordinary case #8
/// exists for -- the policy is a PROMPT and the human takes a second to
/// answer it -- and it is what tells a converging handshake apart from one
/// that only looks converged because a local signer beat the network.
#[derive(Debug, Clone, Copy)]
pub enum Policy {
    Allow(Duration),
    Deny(&'static str),
}

struct AppPolicy {
    behavior: Policy,
}

impl AuthPolicy for AppPolicy {
    fn evaluate(&self, _request: AuthPolicyRequest) -> AuthPolicyOp {
        match self.behavior {
            Policy::Deny(reason) => AuthPolicyOp::deny(reason),
            Policy::Allow(delay) if delay.is_zero() => AuthPolicyOp::allow(),
            Policy::Allow(delay) => {
                let (sender, op) = AuthPolicyOp::pending_channel();
                std::thread::spawn(move || {
                    std::thread::sleep(delay);
                    let _ = sender.resolve(Ok(nmp::AuthPolicyDecision::Allow));
                });
                op
            }
        }
    }
}

/// One measured configuration: a relay shape and an app policy.
#[derive(Debug, Clone, Copy)]
pub struct Case {
    pub label: &'static str,
    pub challenge: Challenge,
    /// Whether the relay accepts the client's kind:22242.
    pub accept_auth: bool,
    pub policy: Policy,
}

/// The configurations worth telling apart.
#[must_use]
pub fn matrix() -> Vec<Case> {
    vec![
        Case {
            label: "challenges on connect",
            challenge: Challenge::OnConnect,
            accept_auth: true,
            policy: Policy::Allow(Duration::ZERO),
        },
        Case {
            label: "challenges on REQ (strfry)",
            challenge: Challenge::OnRequest,
            accept_auth: true,
            policy: Policy::Allow(Duration::ZERO),
        },
        Case {
            label: "challenges on REQ, slow prompt",
            challenge: Challenge::OnRequest,
            accept_auth: true,
            policy: Policy::Allow(Duration::from_millis(900)),
        },
        Case {
            label: "demands auth, never challenges",
            challenge: Challenge::Never,
            accept_auth: true,
            policy: Policy::Allow(Duration::ZERO),
        },
        Case {
            label: "the app's policy refuses",
            challenge: Challenge::OnConnect,
            accept_auth: true,
            policy: Policy::Deny("the user declined to identify to this relay"),
        },
        Case {
            label: "the relay refuses the AUTH",
            challenge: Challenge::OnConnect,
            accept_auth: false,
            policy: Policy::Allow(Duration::ZERO),
        },
    ]
}

/// What one gated read actually did.
#[derive(Debug, Clone)]
pub struct Observed {
    /// Did an `["AUTH", <event>]` reach the socket? This is the fact the
    /// claim above stands or falls on.
    pub auth_frame_sent: bool,
    /// How many `REQ` frames the client sent. A working read path sends the
    /// gated one and then re-issues after authenticating.
    pub reqs: usize,
    /// Every distinct per-relay status the app saw, in first-seen order.
    pub statuses: Vec<SourceStatus>,
    /// The terminal per-relay status.
    pub terminal: Option<SourceStatus>,
    /// The AUTH phase the diagnostics snapshot reports for the relay.
    pub phase: Option<AuthDiagnosticsPhase>,
    /// The reason the engine recorded for a denial, if it exposes one.
    pub denial: Option<String>,
    /// Client frames the relay logged, verbatim, truncated for printing.
    pub frames: Vec<String>,
    /// Which configuration produced this.
    pub label: &'static str,
    /// Who the QUERY surface says refused, if anyone. A bare `AuthDenied`
    /// could not answer this.
    pub denial_source: Option<nmp::AuthDenialSource>,
}

impl std::fmt::Display for Observed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "  {}", self.label)?;
        writeln!(
            f,
            "  AUTH frame reached the relay: {}",
            self.auth_frame_sent
        )?;
        writeln!(f, "  REQ frames from the client:  {}", self.reqs)?;
        writeln!(f, "  status track:                {:?}", self.statuses)?;
        writeln!(f, "  terminal status:             {:?}", self.terminal)?;
        writeln!(f, "  diagnostics phase:           {:?}", self.phase)?;
        writeln!(f, "  denied by:                   {:?}", self.denial_source)?;
        writeln!(f, "  denial reason:               {:?}", self.denial)?;
        for frame in &self.frames {
            let shown: String = frame.chars().take(96).collect();
            writeln!(f, "  client -> relay: {shown}")?;
        }
        Ok(())
    }
}

/// Open one authenticated read against a read-gating relay and report.
pub fn run(case: &Case, budget: Duration) -> Result<Observed, nmp::EngineError> {
    let relay = crate::gated_relay::GatedRelay::start(case.challenge, case.accept_auth)
        .expect("loopback relay binds");
    let relay_url = RelayUrl::parse(relay.url()).expect("a well-formed relay url");

    let config = EngineConfig {
        store_path: None,
        app_relays: vec![relay.url().to_string()],
        ..EngineConfig::default()
    };
    let engine = Engine::new(config)?;
    let reader: PublicKey = engine
        .add_private_key_account(&[17u8; 32], true)
        .expect("a valid secret key")
        .public_key;
    let _policy = engine.add_auth_policy(
        reader,
        AppPolicy {
            behavior: case.policy,
        },
    )?;

    let mut demand = Demand::new(
        Filter {
            kinds: Some([1u16].into_iter().collect()),
            ..Filter::default()
        },
        ReadRouting::Explicit(vec![relay_url.clone()]),
    )
    .expect("one relay is a nonempty explicit set");
    demand.authenticate_as = Some(reader);

    let subscription = engine.observe(LiveQuery::single(demand), None)?;

    let mut statuses: Vec<SourceStatus> = Vec::new();
    let deadline = Instant::now() + budget;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let Ok(frame) = subscription.recv_timeout(remaining.min(Duration::from_millis(250))) else {
            continue;
        };
        for evidence in &frame.evidence {
            for source in &evidence.sources {
                if statuses.last() != Some(&source.status) {
                    statuses.push(source.status);
                }
            }
        }
        // A terminal AUTH answer is the end of the interesting part; give the
        // engine one more beat to re-REQ if it is going to.
        if matches!(statuses.last(), Some(SourceStatus::Requesting)) {
            break;
        }
    }

    let (phase, denial) = read_auth_diagnostics(&engine, &relay_url);
    let observed = Observed {
        label: case.label,
        denial_source: statuses.iter().rev().find_map(|status| match status {
            SourceStatus::AuthDenied { source } => Some(*source),
            _ => None,
        }),
        auth_frame_sent: relay.saw_client_auth(),
        reqs: relay.req_count(),
        statuses: statuses.clone(),
        terminal: statuses.last().copied(),
        phase,
        denial,
        frames: relay.client_frames(),
    };
    drop(subscription);
    engine.shutdown();
    Ok(observed)
}

/// The AUTH row an app can actually reach, and the reason on it.
fn read_auth_diagnostics(
    engine: &Engine,
    relay: &RelayUrl,
) -> (Option<AuthDiagnosticsPhase>, Option<String>) {
    let Ok(diagnostics) = engine.observe_diagnostics() else {
        return (None, None);
    };
    let Some(snapshot) = diagnostics.recv() else {
        return (None, None);
    };
    snapshot
        .auth_sessions
        .iter()
        .find(|session| &session.relay == relay)
        .map_or((None, None), |session| {
            (Some(session.phase), session.denial_reason.clone())
        })
}
