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
//! - [`Engine::observe`] -- a live query (and an optional [`Window`]) in, a
//!   [`Subscription`] streaming [`Frame`]s out.
//! - [`Engine::publish`] -- a [`WriteIntent`] in, a `Receiver<`[`WriteStatus`]`>`
//!   out.
//!
//! Plus identity and signer lifecycle ([`Engine::add_account`],
//! [`Engine::add_signer`], [`Engine::remove_signer`], and
//! [`Engine::set_active_account`]), [`Engine::observe_diagnostics`], and
//! [`Engine::shutdown`]. Every verb fails closed with
//! `EngineError::EngineClosed` once `shutdown` has run -- see [`Engine`]'s
//! own doc for the serialized lifecycle gate that makes this true even under
//! concurrent use, and its `Drop` impl for the case where a caller never
//! calls `shutdown` at all. [`Engine::reset_persistent_store`] is the explicit
//! destructive recovery/trust-domain boundary. It refuses a live in-process
//! engine using the same canonical path; cross-process exclusion remains a
//! separate deployment concern.
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
//!
//! External [`SigningCapability`] implementations are supported: the engine's
//! promotion boundary independently verifies every returned event against the
//! frozen accepted template before it can reach the wire.
//!
//! This crate re-exports every value type an app needs to drive the two
//! nouns, and to name every `DiagnosticsSnapshot` field, without reaching
//! past it -- that re-export list below IS the product surface. It is
//! proved by `nmp-consumer-check`, a separate crate whose `Cargo.toml`
//! depends on `nmp` alone.

mod config;
mod engine;
mod error;
mod relay_information;
mod subscription;

pub use config::EngineConfig;
#[doc(hidden)]
pub use engine::NativeTaskCancel;
pub use engine::RelayInformationRequestError;
pub use engine::{Engine, SignEventRequest};
pub use error::EngineError;
#[doc(hidden)]
pub use nmp_executor::{
    Census as NativeTaskCensus, Executor as NativeTaskExecutor,
    Reservation as NativeTaskReservation, StartedTask as StartedNativeTask,
};
pub use relay_information::{
    RelayInformationCachePolicy, RelayInformationDocument, RelayInformationError,
    RelayInformationFreshness, RelayInformationLimitations, RelayInformationSnapshot,
};
pub use subscription::{
    DiagnosticsSubscription, Frame, ObservationCancel, RequestRowsError, Subscription, Window,
    WindowContents, WindowHandle,
};

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

/// Apply NMP's secure default classification to a network-authored relay
/// hint before a protocol module promotes it into an explicit acquisition
/// candidate. Public hosts are accepted; loopback, private/link-local, and
/// onion hosts are rejected. Operator-configured relays do not use this
/// predicate because their provenance is explicit local configuration.
#[must_use]
pub fn admits_network_relay_hint(relay: &nostr::RelayUrl) -> bool {
    matches!(
        nmp_transport::classify_relay_host(relay),
        nmp_transport::RelayHostClass::Public
    )
}

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
pub use nmp_engine::runtime::{
    ReceiptReattachment, ReceiptStream, SignEventCancel, SignEventError, SignEventOperation,
    SignerRegistration,
};
pub use nmp_grammar::{Durability, WriteIntent, WritePayload, WriteRouting};

// Read outputs `Subscription`/`DiagnosticsSubscription` deliver -- every
// field type `DiagnosticsSnapshot` names must be reachable from here too,
// or an app cannot even print what it read. (The public per-session
// AUTH-diagnostics projection -- `AuthDiagnosticsSnapshot`/
// `AuthDiagnosticsPhase` -- is intentionally NOT part of the supported
// facade surface yet: #8 U4 gives `DiagnosticsSnapshot` an engine-owned
// `auth_sessions` read-out for its own falsifiers/capstone, but the field is
// `#[doc(hidden)]` and its types are not re-exported here. The documented
// facade/FFI auth-diagnostics read-out lands with the app-facing policy
// API's wave.)
//
// Two distinct coverage surfaces live here, deliberately not conflated
// (`docs/design/scoped-evidence-49-12-plan.md` §4): `AcquisitionEvidence`
// (+ `SourceEvidence`/`SourceStatus`/`AuthPhase`/`ShortfallFact`) is the
// scoped, per-query acquisition evidence delivered on every `Frame` --
// per-source facts, never a collapsed completeness verdict.
// `FilterCoverageEntry.coverage` (an `Option<CoverageInterval>`) is the
// engine-global, per-(relay, filter) diagnostics watermark -- unscoped by
// design, and never reused as a query-level verdict either.
pub use nmp_engine::core::{
    AcquisitionEvidence, AuthPhase, DiagnosticsSnapshot, FilterCoverageEntry,
    RelayDiagnosticsSnapshot, Row, RowDelta, ShortfallFact, SourceEvidence, SourceStatus,
    WindowLoad,
};
pub use nmp_router::Lane;
pub use nmp_store::CoverageInterval;

// Value types every verb above is expressed in terms of, including what an
// app needs to build the `WritePayload::Unsigned` template `Engine::publish`
// accepts (`UnsignedEvent::new` takes exactly these four plus a `PublicKey`,
// already re-exported below).
pub use nostr::{Event, EventId, Kind, PublicKey, RelayUrl, Tag, Timestamp, UnsignedEvent};

// Supported signer/provider surface. The engine's promotion boundary now
// validates every external signer result against the frozen accepted event.
pub use nmp_signer::{
    known_local_signers, LocalSignerApp, LocalSignerProtocol, Nip46ClientMetadata,
    Nip46ConnectionEvent, Nip46Error, Nip46Invitation, Nip46Signer, PendingSignerResolveError,
    PendingSignerSender, SignerError, SignerOp, SigningCapability,
};

#[cfg(test)]
mod relay_hint_tests {
    use super::*;

    #[test]
    fn network_hint_gate_rejects_local_and_onion_but_keeps_public_hosts() {
        assert!(admits_network_relay_hint(
            &RelayUrl::parse("wss://relay.example.com").unwrap()
        ));
        assert!(!admits_network_relay_hint(
            &RelayUrl::parse("ws://127.0.0.1:7777").unwrap()
        ));
        assert!(!admits_network_relay_hint(
            &RelayUrl::parse("ws://10.0.0.9").unwrap()
        ));
        assert!(!admits_network_relay_hint(
            &RelayUrl::parse("wss://hiddenservice.onion").unwrap()
        ));
    }
}

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
