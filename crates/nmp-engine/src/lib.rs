//! The deterministic engine: `EngineCore`, its reducer-coupled satellites,
//! and nothing that could make it nondeterministic.
//!
//! `EngineCore` is a synchronous reducer. Its whole interface is
//! `handle(EngineMsg) -> Vec<Effect>` and `tick(Timestamp) -> Vec<Effect>`:
//! messages in, effects out, with concrete redb I/O in between. It owns no
//! threads, no sockets, and no imposed runtime.
//!
//! That sentence used to live in a comment in `nmp/src/lib.rs`, where only
//! review kept it true. Here it is the manifest: this package names no
//! `tokio`, no `crossbeam-channel`, no `futures-channel`, no `reqwest`, and
//! adding one is a reviewable line in `Cargo.toml` rather than an import
//! nobody notices. The async edge that interprets these effects is
//! `nmp-runtime`; the HTTP that acquires NIP-11 documents is `nmp-nip11`.
//! Every headless falsifier in the workspace rests on the reducer being a
//! pure function of its inputs, so the boundary is worth a package.
//!
//! - [`mod@core`] — the reducer and durable-state owner.
//! - [`mod@publish_queue`] — the write-fact/receipt vocabulary the reducer
//!   mints.
//!
//! Callers are `nmp-runtime` and the `nmp` facade's own re-export list. They
//! name this crate directly; `nmp`'s `#[doc(hidden)] pub mod mechanism`
//! basement is gone.

pub mod core;
pub mod publish_queue;
