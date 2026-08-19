//! Every scenario this crate ships, and the mutations that must break each.
//!
//! A scenario is a claim about what crossed a socket, settled against the
//! relay's OWN record rather than against NMP's report of itself.

pub mod auth;
pub mod clock;
pub mod durability;
#[cfg(feature = "external-relay")]
pub mod external;
pub mod framing;
pub mod read;
pub mod socket;
pub mod write;

use crate::scenario::Scenario;
use crate::scenario_entry;

/// The registry. Order is the order `lab all` runs them: cheap and local
/// first, then the ones that pay real reconnect wall-clock, then the ones
/// that need another process.
#[must_use]
pub fn all() -> Vec<Scenario> {
    #[allow(unused_mut)]
    let mut scenarios = vec![
        scenario_entry!("framing", "the websocket layer, checked against an external oracle", &["wrong-guid", "skip-unmasked-check"], framing::run),
        scenario_entry!("truncation", "forty of a hundred served, and EOSE says that is all", &["serve-41"], read::truncation),
        scenario_entry!("never-eose", "the stored phase is never terminated", &["send-eose"], read::never_eose),
        scenario_entry!("eose-then-more", "events after the end of stored events", &[], read::eose_then_more),
        scenario_entry!("closed-midstream", "CLOSED partway through, in the relay's own words", &[], read::closed_midstream),
        scenario_entry!("filter-mismatch", "an event nobody asked for, refused as a row", &[], read::filter_mismatch),
        scenario_entry!("dishonest-bodies", "a forgery and a bad signature beside an honest event", &["serve-honestly"], read::dishonest_bodies),
        scenario_entry!("challenge-midstream", "NIP-42 in the middle of a live subscription", &[], read::challenge_midstream),
        scenario_entry!("challenge-at-connect", "an unsolicited challenge, recorded and not answered", &[], read::challenge_at_connect),
        scenario_entry!("rate-limit-midstream", "a NOTICE and a stop, without CLOSED or EOSE", &[], read::rate_limit_midstream),
        scenario_entry!("delay", "the relay holds the answer, then gives it", &[], read::delay),
        scenario_entry!("accepted-never-served", "OK: true, and the relay keeps nothing", &["actually-store-it"], write::accepted_never_served),
        scenario_entry!("stores-what-it-acknowledges", "the honest control for the above", &[], write::stores_what_it_acknowledges),
        scenario_entry!("refusal-taxonomy", "every NIP-01 prefix, as the fact NMP derives", &[], write::refusal_taxonomy),
        scenario_entry!("accepted-with-message", "OK: true carrying a sentence", &[], write::accepted_with_message),
        scenario_entry!("unanswered-write", "a write nobody answers never resolves", &[], write::unanswered_write),
        scenario_entry!("write-verification", "id and schnorr checked by default; opting out is visible", &[], write::write_verification),
        scenario_entry!("transient-failure", "misbehave once, then behave", &[], write::transient_failure),
        scenario_entry!("mid-frame-truncation", "a real EVENT frame cut after twelve octets", &["send-whole-frame"], socket::mid_frame_truncation),
        scenario_entry!("stalled-direction", "quiet without closing", &[], socket::stalled_direction),
        scenario_entry!("socket-drop", "the TCP connection dropped mid-answer", &[], socket::socket_drop),
        scenario_entry!("captive-portal", "the upgrade answered with a login page", &[], socket::captive_portal),
        scenario_entry!("nip11-document", "one address, two protocols", &[], socket::nip11_document),
        scenario_entry!("silent-subscription-cap", "a ceiling the relay never advertised", &["cap-of-nine"], socket::silent_subscription_cap),
        scenario_entry!("advertised-subscription-cap", "a ceiling NMP can read before it asks", &[], socket::advertised_subscription_cap),
        scenario_entry!("two-engines", "two engines, one relay, two connections", &[], socket::two_engines),
        scenario_entry!("read-gate-unanswered", "NMP does not answer a read-path challenge, and does not say why", &[], auth::read_gate_unanswered),
        scenario_entry!("identity-scoping", "served only what involves your own key", &["drop-scoping"], auth::identity_scoping),
        scenario_entry!("ungated-kind", "only the gated kinds are gated", &[], auth::ungated_kind),
        scenario_entry!("auth-binding", "the challenge and relay tags are actually checked", &[], auth::auth_binding),
        scenario_entry!("stated-instant", "an app states the engine's clock", &["never-state-it"], clock::stated_instant),
        scenario_entry!("backward-jump", "the clock may move backwards", &[], clock::backward_jump),
        scenario_entry!("stated-before-recovery", "the clock is true before the store opens", &[], clock::stated_before_recovery),
        scenario_entry!("unpinned-reads-real-time", "a default config still reads the real clock", &[], clock::unpinned_reads_real_time),
        scenario_entry!("refusal-vs-black-hole", "errno, not elapsed time", &[], durability::refusal_vs_black_hole),
        scenario_entry!("sidecar-reads-during-outage", "durable contents, read with nothing serving them", &[], durability::sidecar_reads_during_outage),
        // Slow: these pay real reconnect wall-clock, which cannot be jumped.
        scenario_entry!("illegal-frame", "a complete illegal frame fails the connection", &[], socket::illegal_frame),
        scenario_entry!("reconnect-replay", "the relay comes back and the subscription replays", &[], socket::reconnect_replay),
        scenario_entry!("transport-is-unmoved", "thirty stated days do not move the backoff", &[], clock::transport_is_unmoved),
        scenario_entry!("gained-while-dead", "the relay gained events while the client was disconnected", &["return-volatile"], durability::gained_while_dead),
        scenario_entry!("acknowledged-survives", "a restart is a restart; the control comes back empty", &[], durability::acknowledged_survives),
    ];

    #[cfg(feature = "external-relay")]
    scenarios.extend([
        scenario_entry!("external-interop", "NMP against a relay nobody here wrote", &[], external::interop),
        scenario_entry!("external-sigkill-restart", "a real process, really killed, whose store survived", &[], external::sigkill_restart),
    ]);

    scenarios
}
