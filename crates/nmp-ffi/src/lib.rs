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
//! mirroring (`convert`/`types`) and the drain-thread bridge from `nmp`'s
//! blocking `recv()` verbs to UniFFI's callback-interface observers.
//!
//! Module layout mirrors the plan's §2 sketch:
//! - [`types`] -- the FFI mirror records/enums (`FfiFilter`/`FfiBinding`/…).
//! - [`convert`] -- `FfiFilter <-> nmp_grammar::Filter` and the
//!   `nostr::Event`/`nmp` value mirrors, plus the shared [`FfiError`](convert::FfiError).
//! - [`observer`] -- the `RowObserver`/`ReceiptObserver` foreign traits.
//! - [`facade`] -- `NmpEngine`/`NmpQueryHandle`, the exported objects.
//! - [`entity`] -- the bech32 nostr-entity DECODE codec (#116), the one
//!   exported free function that needs no `NmpEngine` instance at all: no
//!   engine, no network, no signing.
//!
//! This crate has NO dependency on `nmp-engine` (or any other mechanism
//! crate) at all -- every engine-side value type it mirrors is sourced
//! through `nmp`'s own re-exports (#52 Unit B).

pub mod convert;
pub mod entity;
pub mod facade;
pub mod observer;
pub mod types;

uniffi::setup_scaffolding!();
