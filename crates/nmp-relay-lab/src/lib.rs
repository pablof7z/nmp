//! A scriptable NIP-01 relay: **the test writes the downstream frames.**
//!
//! ```text
//! on a REQ matching P, send these frames, in this order, with these delays
//! ```
//!
//! That one sentence is the mechanism, and it provokes most of the catalogue
//! a real client has to survive. The harness this replaces could only control
//! the client-to-relay direction -- its tap "never sees the relay-to-client
//! direction and never alters the stream" -- and offered six named
//! misbehaviours. Everything else a relay does was, in consequence,
//! *undescribable as a test*, which is a large part of why it was undesigned.
//!
//! # Why not `nostr-relay-builder`
//!
//! Three reasons, in order of weight.
//!
//! 1. **The mechanism is the scenario authoring downstream frames.** A relay
//!    library exists precisely to author them for you; every knob it exposes
//!    is a hole punched back through that, and the set of holes is fixed by
//!    the library rather than by what a scenario needs to say. Its inability
//!    to express accept-but-never-EOSE (which the old harness recorded as a
//!    "deliberate, documented approximation", substituting an outright
//!    `CLOSED`) is one symptom of that, not the disease.
//! 2. **Byte-level control needs the socket.** A frame truncated mid-payload,
//!    injected octets, a direction that stalls without closing, an HTTP 200
//!    login page answering the upgrade -- none of it is reachable from above
//!    a library that owns the socket. [`Step::Bytes`] and
//!    [`Step::PartialFrame`] write straight onto the TCP stream.
//! 3. **Version skew.** `nostr-relay-builder` 0.45.0-alpha.3 re-exports a
//!    DIFFERENT `nostr` than this workspace's pinned 0.44.4, so every keypair
//!    and every seeded event crossed that boundary by hex or JSON round-trip,
//!    and the old fixture carried a `mirror_keys` bridge and a warning
//!    against glob-importing the prelude. Speaking the wire directly against
//!    the pinned `nostr` deletes the bridge: a scenario hands this crate the
//!    same [`nostr::Event`] value the engine will observe.
//!
//! The websocket server half is [`ws`] -- RFC 6455 as far as NIP-01 traffic
//! needs it, with every unhandled case recorded as a fault rather than
//! skipped.
//!
//! # A scenario
//!
//! ```no_run
//! # async fn scenario() {
//! use std::time::Duration;
//! use nmp_relay_lab::{RelayLab, Reply, Req, Script};
//!
//! # let corpus: Vec<nostr::Event> = Vec::new();
//! // A hundred notes exist. The relay serves forty and says EOSE, so the
//! // app is told the relay has finished and is never told it held more.
//! let relay = RelayLab::start(
//!     Script::new()
//!         .seed(corpus)
//!         .on_req(Req::kind(1), Reply::truncate_at(40)),
//! )
//! .await;
//!
//! let engine = nmp::Engine::new(nmp::EngineConfig {
//!     app_relays: vec![relay.url().to_string()],
//!     ..nmp::EngineConfig::default()
//! })
//! .expect("engine builds");
//!
//! // ... open an observation, collect rows ...
//!
//! // The claim is checked against the WIRE, never against NMP's self-report:
//! // the app asked for more than forty and was given forty.
//! relay.wire().wait_quiet(Duration::from_millis(200), Duration::from_secs(5)).await;
//! let record = relay.record();
//! assert!(record.reqs()[0].max_limit().unwrap_or(u64::MAX) > 40);
//! assert_eq!(record.served_event_ids().len(), 40);
//! assert_eq!(record.eosed_subscription_ids().len(), 1);
//! # }
//! ```
//!
//! # The catalogue, one line each
//!
//! | what the relay does | how a scenario says it |
//! |---|---|
//! | truncate silently | `.on_req(Req::any(), Reply::truncate_at(40))` |
//! | never EOSE | `.on_req(Req::any(), Reply::never_eose())` |
//! | EOSE, then more events | `.on_req(Req::any(), Reply::eose_then(later))` |
//! | CLOSED mid-subscription | `Reply::stored().after(d).then_closed("rate-limited: slow down")` |
//! | serve non-matching events | `.on_req(Req::any(), Reply::serve(unrelated))` |
//! | forgery / bad signature | `Reply::new().then_events_json(vec![forge::bad_signature(&e)]).then_eose()` |
//! | AUTH mid-subscription | `Reply::stored().then_auth("challenge-1")` |
//! | rate-limit mid-stream | `Reply::new().then_stored().then_notice("rate-limited: slow down")` |
//! | accept a write, never serve it | `.on_event(Ev::any(), Reply::ok_but_forget())` |
//! | `OK: true` with a message | `.on_event(Ev::any(), Reply::ok_with("this event was replaced"))` |
//! | `OK: false` with a real prefix | `.on_event(Ev::any(), Reply::rejected("duplicate: have this"))` |
//! | delay, per frame or per phase | `Reply::new().after(d).then_stored().after(d).then_eose()` |
//! | truncate a frame mid-way | `Reply::stored().then_partial_event(event, 12)` |
//! | truncate an arbitrary frame | `Reply::new().then_partial_frame(body, 12)` |
//! | inject bytes | `Reply::new().then_bytes(vec![0xff, 0xff])` |
//! | stall the direction | `Reply::new().then_stall()` |
//! | drop the socket | `Reply::new().then_disconnect()` |
//! | captive portal | `.upgrade(Upgrade::Http { status: 200, .. })` |
//! | never answer the upgrade | `.upgrade(Upgrade::Hang)` |
//! | challenge at connect | `.on_connect(Reply::auth("challenge-1"))` |
//! | misbehave once, then behave | `.on_nth_req(1, Req::any(), Reply::closed("error: try again"))` |
//! | advertise NIP-11 limits | `.nip11(Nip11::limits(Some(3), None))` |
//! | cap subscriptions silently | `.cap_subscriptions(3, "error: too many")` |
//!
//! # Honesty
//!
//! Inbound events are verified -- id and schnorr signature -- by default. A
//! relay that admits an unsigned event is not one any client has to survive,
//! and a scenario built on one proves nothing about the real world. Opting
//! out is [`Script::accepts_unverified_writes`], per script, and visible in
//! the scenario that does it.
//!
//! Nothing is verified on the way OUT, ever. That is what makes forgery, a
//! bad signature, and a filter mismatch sayable at all.
//!
//! # Witnessing
//!
//! [`WireLog`] records BOTH directions, decoded, per connection. A guarantee
//! scenario must never take NMP's own report as its only witness: if the
//! engine's diagnostics claim one subscription and a bug in diagnostics is
//! what made the claim, the scenario passes because the thing under test said
//! so. Every count-shaped assertion reads the relay's own record instead.
//!
//! # Two engines, one relay
//!
//! Nothing here is single-client. Each accepted connection gets its own
//! decoder, its own session state, and its own index in the record, so a
//! concurrent-edit scenario points two [`nmp::Engine`]s at one
//! [`RelayLab::url`] and separates their traffic with
//! [`WireRecord::on_connection`].

#![forbid(unsafe_code)]

pub mod clock;
mod relay;
mod script;
mod wire;
pub mod ws;

pub use relay::{wait_reachable, RelayLab};
pub use script::{forge, Ev, Nip11, Reply, Req, ReqFrame, Script, Serve, Step, Upgrade};
pub use wire::{Direction, WireFrame, WireLog, WireRecord, WireReq};
