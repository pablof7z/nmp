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
//! - [`Engine::publish`] -- a [`WriteIntent`] in, a receipt stream of
//!   [`WriteStatus`] out (drained by blocking `recv` or, over the FFI/SDK, an
//!   awaited pull handle).
//!
//! Plus identity, signer, and NIP-42 AUTH-policy lifecycle
//! ([`Engine::add_account`], [`Engine::remove_account`],
//! [`Engine::add_auth_policy`], [`Engine::remove_auth_policy`],
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

mod auth;
mod config;
mod diagnostics;
mod engine;
mod error;
mod relay_information;
mod subscription;

pub use auth::{
    AuthPolicy, AuthPolicyDecision, AuthPolicyError, AuthPolicyOp, AuthPolicyPendingSender,
    AuthPolicyRequest, AuthPolicyResolveError, AuthPolicyResult,
};
pub use config::EngineConfig;
pub use diagnostics::{
    AuthDiagnosticsPhase, AuthDiagnosticsSnapshot, DiagnosticsSnapshot, FilterCoverageEntry,
    RelayDiagnosticsSnapshot,
};
#[doc(hidden)]
pub use engine::NativeTaskCancel;
pub use engine::RelayInformationRequestError;
pub use engine::{
    AccountRegistration, AuthPolicyRegistration, CancelWriteError, CancelWriteOutcome, Engine,
    SignEventRequest,
};
pub use error::EngineError;

/// Monotonic count of real NMP-owned OS threads spawned this process (#680
/// falsifier instrumentation). The thread-scaling falsifier asserts opening
/// many observations leaves this delta at 0: an observation is a lightweight
/// `Arc`+waker, never an OS thread. Doc-hidden test instrumentation, not part
/// of the product surface.
#[doc(hidden)]
#[must_use]
pub fn nmp_threads_spawned() -> u64 {
    nmp_engine::nmp_threads_spawned()
}
// The pull-based async observation surface (#680) is the FFI/SDK delivery
// mechanism — its app contract is documented in `nmp-ffi`'s own surface
// snapshot and the Swift/Kotlin SDKs. The documented direct-Rust product
// surface stays the blocking `Subscription`/`recv()` nouns below; these async
// twins remain fully usable (nmp-ffi and any direct-Rust app await them) but
// are doc-hidden so they do not double the facade snapshot with generic
// auto-trait expansions.
#[doc(hidden)]
pub use nmp_engine::runtime::ConcurrentNext;
#[doc(hidden)]
pub use nmp_executor::{Reservation as NativeTaskReservation, StartedTask as StartedNativeTask};
pub use relay_information::{
    RelayInformationCachePolicy, RelayInformationDocument, RelayInformationError,
    RelayInformationFreshness, RelayInformationLimitations, RelayInformationSnapshot,
};
#[doc(hidden)]
pub use subscription::{AsyncDiagnosticsSubscription, AsyncSubscription};
pub use subscription::{
    DiagnosticsSubscription, Frame, ObservationCancel, RequestRowsError, Subscription, Window,
    WindowContents, WindowHandle,
};

// The grammar an app builds a `LiveQuery`'s `Demand` out of. `Demand`'s
// `selection` is the `Filter`; `source`/`access`/`cache` are the #106 axes
// -- `LiveQuery::from_filter` applies `Demand`'s static default so existing
// `Filter`-only call sites need no source/access reasoning of their own.
pub use nmp_grammar::{
    AccessContext, Binding, CacheMode, Demand, DemandError, Derived, Filter, Freshness,
    IdentityField, IndexedTagName, Selector, SetAlgebra, SetOp, SourceAuthority,
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
pub use nmp_engine::runtime::{
    ReceiptReattachment, ReceiptStream, SignEventCancel, SignEventError, SignEventOperation,
    SignerRegistration,
};
// The receipt/status receiver is delivery mechanism — it was previously an
// external `std::sync::mpsc::Receiver` (never a documented nmp noun); it is now
// the engine-owned waker-aware FIFO `FifoReceiver` (blocking `recv` for direct
// Rust) plus its async `AsyncFifoReceiver` twin. Both stay doc-hidden so the
// documented product surface keeps its previous shape: `publish` returns a
// receipt stream you drain, not a new documented type family.
#[doc(hidden)]
pub use nmp_engine::runtime::{AsyncFifoReceiver, FifoReceiver};
// Producer-side FIFO mechanism, used only by protocol modules (e.g. nmp-nip02's
// follow-action worker) to feed a receipt/status stream — not app product
// surface, so doc-hidden and kept out of the facade snapshot.
#[doc(hidden)]
pub use nmp_engine::runtime::{fifo_channel, FifoSender};
pub use nmp_grammar::{
    CorrelationToken, CorrelationTokenError, Durability, WriteIntent, WritePayload, WriteRouting,
};

// Read outputs `Subscription`/`DiagnosticsSubscription` deliver -- every
// field type `DiagnosticsSnapshot` names must be reachable from here too,
// or an app cannot even print what it read. The diagnostics snapshot family
// itself (`DiagnosticsSnapshot`/`RelayDiagnosticsSnapshot`/
// `FilterCoverageEntry` plus the #8 AUTH read-out
// `AuthDiagnosticsSnapshot`/`AuthDiagnosticsPhase`) is facade-OWNED --
// defined in [`mod@diagnostics`] and exported above, converted once at the
// `DiagnosticsSubscription` delivery boundary -- rather than re-exported
// from the engine (the `bc8fb97` NIP-11 pattern).
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
    AcquisitionEvidence, AuthPhase, Row, RowDelta, ShortfallFact, SourceEvidence, SourceStatus,
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
