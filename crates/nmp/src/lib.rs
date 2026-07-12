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
//! Plus identity ([`Engine::add_account`]/[`Engine::set_active_account`]),
//! [`Engine::observe_diagnostics`], and [`Engine::shutdown`]. Every verb
//! fails closed with `EngineError::EngineClosed` once `shutdown` has run --
//! see [`Engine`]'s own doc for the serialized lifecycle gate that makes
//! this true even under concurrent use, and its `Drop` impl for the case
//! where a caller never calls `shutdown` at all.
//!
//! Everything below `Engine` -- `EngineThread`, `Handle`, `LiveDirectory`,
//! `RedbStore`/`MemoryStore`, `PoolConfig`, `LocalKeySigner` -- is no longer
//! an app contract (#52's "internal or explicitly unstable"). Two things
//! stay behind the `unstable-mechanism` cargo feature, off by default and
//! `#[doc(hidden)]` where applicable -- enabling either is a greppable,
//! reviewable line, not a silent bypass:
//!
//! - `Engine::from_parts`, an in-workspace/test hatch for `nmp-bdd`'s
//!   scripted-relay harness (may freely need mechanism-crate types; it is
//!   not expected to be usable from an `nmp`-only dependency).
//! - `Engine::add_signer`/`SigningCapability` -- a THIRD-PARTY signing
//!   capability's output is not yet validated against the frozen unsigned
//!   template before it reaches the wire (`nmp-engine`'s #2/#3 Unit U3),
//!   so the facade must not present a custom-signer path as supported
//!   until that lands. `Engine::add_account`'s built-in `LocalKeySigner`
//!   path is unaffected -- it signs the frozen template itself.
//!
//! This crate re-exports every value type an app needs to drive the two
//! nouns, and to name every `DiagnosticsSnapshot` field, without reaching
//! past it -- that re-export list below IS the product surface. It is
//! proved by `nmp-consumer-check`, a separate crate whose `Cargo.toml`
//! depends on `nmp` alone.

mod config;
mod engine;
mod error;
mod subscription;

pub use config::EngineConfig;
pub use engine::Engine;
pub use error::EngineError;
pub use subscription::{DiagnosticsSubscription, ObservationCancel, Subscription};

// The grammar an app builds a `LiveQuery`'s `Demand` out of. `Demand`'s
// `selection` is the `Filter`; `source`/`access`/`cache` are the #106 axes
// -- `LiveQuery::from_filter` applies `Demand`'s static default so existing
// `Filter`-only call sites need no source/access reasoning of their own.
pub use nmp_grammar::{
    AccessContext, Binding, CacheMode, Demand, DemandError, Derived, Filter, IdentityField,
    IndexedTagName, Selector, SetAlgebra, SetOp, SourceAuthority,
};
pub use nmp_resolver::LiveQuery;

// Bech32 nostr-entity DECODE (#116) -- npub/nprofile/note/nevent/naddr ->
// hex id/pubkey + relay hints. A pure codec, unrelated to the two nouns
// above, but "shared, protocol-level" per #116's own framing: a direct-Rust
// app gets it here for the identical reason `nmp-ffi` gets it at the FFI
// boundary, rather than each hand-rolling its own bech32 decode.
pub use nmp_grammar::{decode_nostr_entity, NostrEntity, NostrEntityError};

// The write plane a `WriteIntent` is built from, and its receipt stream.
// `Durability`/`WriteIntent`/`WritePayload`/`WriteRouting` moved to
// `nmp-grammar` (#115 Fable ruling, Fork 3) -- a protocol module composing
// a `WriteIntent` must not gain an engine dependency to do so.
// `NarrowOnly`/`PrivateRoute`/`HostAuthority` are deliberately NOT
// re-exported here even though they are `pub` in `nmp-grammar`: `NarrowOnly`'s
// constructor validates only that a set can never widen after construction,
// not that its initial contents are actually private (#22) -- an app must
// not be able to place arbitrary public relays into a route that looks
// structurally narrow; `HostAuthority::from_selected_host` (#115) is
// mintable by ANY string an app hands it, the same shape of foot-gun --
// an app must not be able to assert its own ad-hoc "host" out of thin air.
// A validated, opaque private-route or pinned-host mint belongs in a
// protocol module (e.g. `nmp-nip29::compose_group_send`), not the default
// facade surface.
pub use nmp_engine::core::ReceiptId;
pub use nmp_engine::outbox::WriteStatus;
pub use nmp_grammar::{Durability, WriteIntent, WritePayload, WriteRouting};
pub use nmp_engine::runtime::{ReceiptReattachment, ReceiptStream};

// Read outputs `Subscription`/`DiagnosticsSubscription` deliver -- every
// field type `DiagnosticsSnapshot` names must be reachable from here too,
// or an app cannot even print what it read.
//
// Two distinct coverage surfaces live here, deliberately not conflated
// (`docs/design/scoped-evidence-49-12-plan.md` §4): `AcquisitionEvidence`
// (+ `SourceEvidence`/`SourceStatus`/`AuthPhase`/`ShortfallFact`) is the
// scoped, per-query acquisition evidence delivered alongside every
// `RowsMsg` -- per-source facts, never a collapsed completeness verdict.
// `FilterCoverageEntry.coverage` (an `Option<CoverageInterval>`) is the
// engine-global, per-(relay, filter) diagnostics watermark -- unscoped by
// design, and never reused as a query-level verdict either.
pub use nmp_engine::core::{
    AcquisitionEvidence, AuthPhase, DiagnosticsSnapshot, FilterCoverageEntry,
    RelayDiagnosticsSnapshot, Row, RowDelta, ShortfallFact, SourceEvidence, SourceStatus,
};
pub use nmp_engine::runtime::RowsMsg;
pub use nmp_router::Lane;
pub use nmp_store::CoverageInterval;

// Value types every verb above is expressed in terms of, including what an
// app needs to build the `WritePayload::Unsigned` template `Engine::publish`
// accepts (`UnsignedEvent::new` takes exactly these four plus a `PublicKey`,
// already re-exported below).
pub use nostr::{Event, EventId, Kind, PublicKey, RelayUrl, Tag, Timestamp, UnsignedEvent};

// A lower-level signing capability an app can implement itself (e.g. a
// NIP-46/bunker remote signer) and hand to `Engine::add_signer` -- gated
// behind `unstable-mechanism` until #2/#3's Unit U3 validates a signer's
// output; see this module's doc and `Engine::add_signer`'s own doc.
#[cfg(feature = "unstable-mechanism")]
pub use nmp_signer::SigningCapability;

// The concrete mechanism types are internal by default (#52's "internal or
// explicitly unstable"). `Engine::from_parts` needs `EventStore`/
// `RelayDirectory`/`PoolConfig` in a caller's signature to be usable at
// all, so those three -- and ONLY those three -- are re-exported behind the
// same feature that unlocks the constructor itself. This hatch is an
// in-workspace/test exception (`nmp-bdd`), not required to be usable from
// an `nmp`-only dependency -- it may legitimately need further
// mechanism-crate types that this crate does not re-export.
#[cfg(feature = "unstable-mechanism")]
pub use nmp_router::RelayDirectory;
#[cfg(feature = "unstable-mechanism")]
pub use nmp_store::EventStore;
#[cfg(feature = "unstable-mechanism")]
pub use nmp_transport::PoolConfig;
