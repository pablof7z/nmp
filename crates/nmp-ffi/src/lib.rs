//! `nmp-ffi` -- the UniFFI boundary crate (M4 plan §1/§2): the minimal
//! two-noun surface (live query, write intent) plus diagnostics, exported as
//! native Swift (and, later, Kotlin -- M6) values via UniFFI's proc-macro
//! mode (no `.udl` file). Nothing in the workspace depends on this crate;
//! it wraps [`nmp::Engine`] (#52) and is the top of the graph, replacing
//! what would otherwise be an app's own hand-rolled FFI layer.
//!
//! Everything semantic -- construction, store/directory selection, the
//! router cap, and the caller-supplied-`Signed` verify -- lives in `nmp`
//! (and, for the verify, `nmp-engine::core::EngineCore::on_publish`'s
//! acceptance boundary) so this crate inherits it rather than re-deriving
//! it (see [`facade`]'s doc). What genuinely stays FFI-boundary work: type
//! mirroring (`convert`/`types`) and the pull-based async UniFFI object
//! handles (`NmpRowStream`/`NmpDiagnosticsStream`/`NmpReceiptStream`/…) whose
//! `next()` awaits `nmp`'s waker-driven async observation surface (#680) —
//! costing zero NMP-owned OS threads per observation.
//!
//! Module layout mirrors the plan's §2 sketch:
//! - [`auth`] -- opaque account/policy registrations and the completion-only
//!   foreign AUTH-policy callback bridge.
//! - [`types`] -- the FFI mirror records/enums (`FfiFilter`/`FfiBinding`/…).
//! - [`convert`] -- `FfiFilter <-> nmp_grammar::Filter` and the
//!   `nostr::Event`/`nmp` value mirrors, plus the shared [`FfiError`](convert::FfiError).
//! - [`facade`] -- `NmpEngine` plus the pull-based async stream objects
//!   (`NmpRowStream`/`NmpDiagnosticsStream`/`NmpReceiptStream`/
//!   `NmpSignEventHandle`), the exported objects.
//! - [`entity`] -- the bech32 nostr-entity DECODE codec (#116), the one
//!   exported free function that needs no `NmpEngine` instance at all: no
//!   engine, no network, no signing.
//! - [`nip51`] -- tolerant, observational NIP-51 Simple-groups parsing
//!   (#863). A free function over a caller-constructible `FfiRow` returning
//!   plain data; no observation-qualified wrapper, projection error, or
//!   frame proof exists here, and none may be reintroduced
//!   (`scripts/check-nip51-no-derived-authority.sh`).
//! - [`nip29`] -- the read-only NIP-29 host-browser projection (#108):
//!   `nmp-nip29`'s group-discovery constructor as a top-level free function,
//!   same "no `NmpEngine` instance needed" shape as [`entity`]. NIP-29 does
//!   not project a fixed content catalog or a kind:9 composer (#838).
//! - [`blossom`] -- the opt-in Blossom blob projection (#555): kind:24242
//!   authorization drafts/validation and the blocking BUD-02/04/12 client,
//!   engine-less like [`entity`]/[`nip29`], with each operation's failure
//!   taxonomy crossing as its own typed error enum.
//! - [`nip22`] -- typed NIP-22 comments over NIP-73 external targets
//!   (#572): root-thread demand and decode as top-level free functions
//!   (`nmp::nip22` needs no engine dependency at all -- `comment_intent`
//!   takes its author/time as explicit caller parameters). The composer
//!   returns an ordinary [`types::FfiWriteIntent`]; generic
//!   [`facade::NmpEngine::publish`] owns the receipt lifecycle.
//!
//! This crate has NO production dependency on `nmp-engine`, `nmp-grammar`,
//! `nmp-signer`, or any other mechanism crate at all (#851) -- every
//! engine, query, receipt, signer and typed write value it mirrors is
//! sourced through `nmp`'s own re-exports (#52 Unit B), including the
//! NIP-22 comment vocabulary, which it reaches by enabling the facade's
//! `nip22` feature rather than by a second edge to `nmp-nip22`.
//! `scripts/check-ffi-facade-boundary.sh` is the mechanism that keeps that
//! true. `nmp-nip51`/`nmp-nip29` (see [`nip29`]'s own doc) and
//! `nmp-blossom` (#555, see [`blossom`]'s) are the opt-in protocol
//! dependencies projected by this boundary.

pub mod auth;
pub mod blossom;
pub mod content;
pub mod convert;
pub mod entity;
pub mod facade;
pub mod nip02;
pub mod nip22;
pub mod nip29;
pub mod nip51;
pub mod signer;
// #1243: the one tagging door at the native boundary -- reply, chat reply and
// repost, each returning the `FfiEventBuilder` the publish door already takes.
pub mod tagging;
pub mod types;

uniffi::setup_scaffolding!();
