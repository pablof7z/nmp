//! `nmp-ffi` -- the UniFFI boundary crate (M4 plan §1/§2): the minimal
//! two-noun surface (live query, write intent) plus diagnostics, exported as
//! native Swift (and, later, Kotlin -- M6) values via UniFFI's proc-macro
//! mode (no `.udl` file). Nothing in the workspace depends on this crate;
//! it depends on `nmp-engine` and is the top of the graph, replacing what
//! would otherwise be an app's own hand-rolled FFI layer.
//!
//! Module layout mirrors the plan's §2 sketch:
//! - [`types`] -- the FFI mirror records/enums (`FfiFilter`/`FfiBinding`/…).
//! - [`convert`] -- `FfiFilter <-> nmp_grammar::Filter` and the
//!   `nostr::Event`/`nmp_engine` value mirrors, plus the shared [`FfiError`](convert::FfiError).
//! - [`observer`] -- the `RowObserver`/`ReceiptObserver` foreign traits.
//! - [`facade`] -- `NmpEngine`/`NmpQueryHandle`, the exported objects.

pub mod convert;
pub mod facade;
pub mod observer;
pub mod types;

uniffi::setup_scaffolding!();
