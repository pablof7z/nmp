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
//! - [`mod@negentropy`] — the NIP-77 prober FSM, the `ProbedRelay` capability
//!   token, and the `Reconciler` the reducer drives turn by turn. A module
//!   rather than its own crate: the reducer holds `Prober` and `Reconciler`
//!   as its own fields and matches `NegStep` directly, so a crate line would
//!   move the `negentropy` dependency one hop rather than out.
//! - [`mod@publish_queue`] — the write-fact/receipt vocabulary the reducer
//!   mints.
//! - [`mod@ingest_attribution`] — bench-only ingest counters.
//!
//! Callers are `nmp-runtime`, the `nmp` facade's own re-export list, and the
//! in-workspace harnesses (`nmp-bdd`, `nmp/tests`) that drive a real reducer
//! headlessly. Those harnesses used to reach it through `nmp`'s
//! `#[doc(hidden)] pub mod mechanism` basement; they name this crate now, and
//! the basement is gone.

pub mod core;
#[cfg(feature = "bench-instrumentation")]
pub mod ingest_attribution;
pub mod negentropy;
pub mod publish_queue;
