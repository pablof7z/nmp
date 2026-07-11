//! `nmp` -- THE supported Rust product surface (#52,
//! `docs/design/canonical-facade-52-plan.md`). Every direct-Rust app and
//! `nmp-ffi` both depend on this crate alone; the mechanism crates
//! (`nmp-store`, `nmp-router`, `nmp-transport`, `nmp-resolver`, `nmp-signer`)
//! are internal implementation detail behind it, present only transitively.
//!
//! Two nouns, one construction call:
//!
//! - [`Engine::new`] -- config in, a running engine out. Owns
//!   config -> store/directory selection and the router cap that
//!   `nmp-ffi`/`nmp-demo` used to each assemble by hand.
//! - [`Engine::observe`] -- a live query in, a [`Subscription`] streaming
//!   [`RowsMsg`] out.
//! - [`Engine::publish`] -- a [`WriteIntent`] in, a `Receiver<`[`WriteStatus`]`>`
//!   out.
//!
//! Plus identity ([`Engine::add_account`]/[`Engine::set_active_account`]/
//! [`Engine::add_signer`]), [`Engine::observe_diagnostics`], and
//! [`Engine::shutdown`].
//!
//! Everything below `Engine` -- `EngineThread`, `Handle`, `LiveDirectory`,
//! `RedbStore`/`MemoryStore`, `PoolConfig`, `LocalKeySigner` -- is no longer
//! an app contract (#52's "internal or explicitly unstable"). The one
//! sanctioned exception is [`Engine::from_parts`], gated behind the
//! `unstable-mechanism` cargo feature for `nmp-bdd`'s scripted-relay test
//! harness; enabling it is a greppable, reviewable line, not a silent
//! bypass.
//!
//! This crate re-exports every value type an app needs to drive the two
//! nouns without reaching past it -- that re-export list below IS the
//! product surface.

mod config;
mod engine;
mod error;
mod subscription;

pub use config::EngineConfig;
pub use engine::Engine;
pub use error::EngineError;
pub use subscription::{DiagnosticsSubscription, Subscription};

// The grammar an app builds a `LiveQuery`'s `Filter` out of.
pub use nmp_grammar::{
    Binding, Derived, Filter, IdentityField, Selector, SetAlgebra, SetOp, TagName,
};
pub use nmp_resolver::LiveQuery;

// The write plane a `WriteIntent` is built from, and its receipt stream.
pub use nmp_engine::outbox::{
    Durability, NarrowOnly, PrivateRoute, WriteIntent, WritePayload, WriteRouting, WriteStatus,
};

// Read outputs `Subscription`/`DiagnosticsSubscription` deliver.
pub use nmp_engine::core::{DiagnosticsSnapshot, QueryCoverage, RowDelta};
pub use nmp_engine::runtime::RowsMsg;

// A lower-level signing capability an app can implement itself (e.g. a
// NIP-46/bunker remote signer) and hand to `Engine::add_signer`.
pub use nmp_signer::SigningCapability;

// Value types every verb above is expressed in terms of.
pub use nostr::{Event, EventId, PublicKey, RelayUrl};

// The concrete mechanism types are internal by default (#52's "internal or
// explicitly unstable"). `Engine::from_parts` needs `EventStore`/
// `RelayDirectory`/`PoolConfig` in a caller's signature to be usable at
// all, so those three -- and ONLY those three -- are re-exported behind the
// same feature that unlocks the constructor itself.
#[cfg(feature = "unstable-mechanism")]
pub use nmp_router::RelayDirectory;
#[cfg(feature = "unstable-mechanism")]
pub use nmp_store::EventStore;
#[cfg(feature = "unstable-mechanism")]
pub use nmp_transport::PoolConfig;
