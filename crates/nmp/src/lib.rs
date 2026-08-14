//! `nmp` -- THE supported Rust product API (#52,
//! `docs/design/canonical-facade-52-plan.md`). Every direct-Rust app and
//! `nmp-ffi` both depend on this crate alone; the mechanism crates
//! (`nmp-store`, `nmp-router`, `nmp-transport`, `nmp-resolver`, `nmp-signer`,
//! `nmp-local-signer`) are internal implementation detail behind it, present
//! only transitively.
//!
//! Two nouns, one construction call:
//!
//! - [`Engine::new`] -- config in, a running engine out. Owns
//!   config -> store/neutral-routing-fact selection and the router cap that
//!   `nmp-ffi` used to assemble by hand.
//! - [`Engine::observe`] -- a live query (and an optional [`Window`]) in, a
//!   [`Subscription`] streaming [`Frame`]s out.
//! - [`Engine::publish`] -- a [`WriteIntent`] in, a receipt stream of
//!   [`WriteFact`] out (drained by blocking `recv` or, over the FFI/SDK, an
//!   awaited pull handle).
//!
//! Plus whole-session account and NIP-42 AUTH-policy lifecycle
//! ([`Engine::session`], [`Engine::add_private_key_account`],
//! [`Engine::add_public_key_account`], [`Engine::make_current_account`],
//! [`Engine::remove_account`], [`Engine::clear_session`],
//! [`Engine::add_auth_policy`], and [`Engine::remove_auth_policy`]),
//! [`Engine::observe_diagnostics`], and [`Engine::shutdown`]. Every verb fails closed with
//! `EngineError::EngineClosed` once `shutdown` has run -- see [`Engine`]'s
//! own doc for the serialized lifecycle gate that makes this true even under
//! concurrent use, and its `Drop` impl for the case where a caller never
//! calls `shutdown` at all. [`Engine::reset_persistent_store`] is the explicit
//! destructive recovery/trust-domain boundary. It refuses a live in-process
//! engine using the same canonical path; cross-process exclusion remains a
//! separate deployment concern.
//!
//! Everything below `Engine` -- `EngineThread`, `Handle`,
//! `RedbStore`, `PoolConfig`, `LocalKeySigner` -- is no longer
//! an app contract (#52's "internal or explicitly unstable"). Two things
//! stay behind the `unstable-mechanism` cargo feature, off by default and
//! `#[doc(hidden)]` where applicable -- enabling either is a greppable,
//! reviewable line, not a silent bypass:
//!
//! - `Engine::from_parts`, an in-workspace/test hatch for `nmp-bdd`'s
//!   scripted-relay harness (may freely need mechanism-crate types; it is
//!   not expected to be usable from an `nmp`-only dependency).
//!
//! This crate re-exports every value type an app needs to drive the two
//! nouns, and to name every `DiagnosticsSnapshot` field, without reaching
//! past it -- that re-export list below IS the public API. It is
//! proved by `nmp-consumer-check`, a separate crate whose `Cargo.toml`
//! depends on `nmp` alone.

mod auth;
mod config;
mod diagnostics;
mod engine;
mod error;
mod observation;
mod relay_information;
mod subscription;

// #827: the M3 engine, folded in from the former `nmp-engine` crate. These are
// the SAME modules, at the same names, moved verbatim -- the crate boundary
// they used to sit behind added nothing but a second public API.
//
// - [`mod@core`] -- `EngineCore`: the PURE synchronous reducer. No I/O, no
//   threads, no imposed runtime. Its whole interface is
//   `handle(EngineMsg) -> Vec<Effect>` / `tick(Timestamp) -> Vec<Effect>`.
//   This is what keeps the whole engine headlessly testable (plan §2
//   position 1).
// - [`mod@runtime`] -- the async edge: `EngineThread` (one dedicated OS
//   thread, blocking `mpsc` recv loop, D8) + `Handle` (the cheap
//   `Clone + Send` value the app holds).
// - [`mod@delivery`] -- the write-intent/receipt plane (durability class, typed
//   routing, the receipt stream).
// - [`mod@negentropy`] -- the prober FSM + `ProbedRelay` capability token +
//   `Reconciler` (a MODULE, not a crate -- plan §1: reducer-coupled).
// - [`mod@relay_information_service`] -- engine-owned one-shot NIP-11
//   acquisition. Public NIP-11 values live in [`mod@relay_information`];
//   this module owns cache, flight, and fetch coordination only.
//
// They are PRIVATE, exactly like every other module of this facade, which is
// what keeps ~242 mechanism items out of the public API: the
// public API stays the selective `pub use crate::core::…` /
// `crate::runtime::…` list below, byte-for-byte the list that used to read
// `nmp_engine::core::…`. Making them `pub` (even `#[doc(hidden)]`) would
// instead dump the whole mechanism into the facade.
mod core;
#[cfg(feature = "bench-instrumentation")]
mod ingest_attribution;
/// Test-only durable-store double. It lives outside `core` on purpose: it is
/// a store implementation, not reducer code.
#[cfg(test)]
mod lane_fault_store;
mod negentropy;
mod publish_queue;
mod relay_information_service;
mod replaceable_materializer;
mod runtime;
mod session;

/// The doc-hidden mechanism door, for IN-WORKSPACE harnesses only.
///
/// The reducer and the runtime used to be reachable as `nmp_engine::core` /
/// `nmp_engine::runtime` because they lived in another crate. Three kinds of
/// caller genuinely drive them directly and always did: this crate's own
/// headless reducer tests and runtime integration tests (`tests/`), its
/// benchmark examples, and the in-workspace harnesses (`nmp-bdd`, and the
/// remote-signer provider's restart falsifiers) that spawn a real
/// `EngineThread`. Folding the crate in must not silently delete those
/// falsifiers, so the same access survives here -- through ONE explicitly
/// named door rather than by making five modules public.
///
/// `#[doc(hidden)]`, so rustdoc omits it and everything beneath it: nothing
/// here is an app contract. An application uses the re-exported public API
/// below; if something in here is genuinely needed by an app, the answer is
/// to project it through that API, not to reach in.
#[doc(hidden)]
pub mod mechanism {
    pub mod core {
        pub use crate::core::*;
    }
    #[cfg(feature = "bench-instrumentation")]
    pub mod ingest_attribution {
        pub use crate::ingest_attribution::*;
    }
    pub mod negentropy {
        pub use crate::negentropy::*;
    }
    pub mod publish_queue {
        pub use crate::publish_queue::*;
    }
    pub mod relay_information_service {
        pub use crate::relay_information_service::*;
    }
    pub mod runtime {
        pub use crate::runtime::*;
    }
}

// #851: the NIP-22 comment vocabulary and its write operation, owned here so
// direct Rust and `nmp-ffi` cannot end up with two owners of the same values.
// Behind the `nip22` cargo feature: an app that never composes a comment does
// not link the mechanism crate.
#[cfg(feature = "nip22")]
pub mod nip22;

// #155: NIP-25 reactions, projected here for the same reason NIP-22 is --
// #1239 records four protocol families that `nmp-ffi` reaches and the `nmp`
// facade does not, so a direct-Rust app needs a second Cargo dependency for
// something a Swift app gets for free. A new family is wired through the
// facade at birth rather than added to that list. Behind the `nip25` feature:
// an app that never composes a reaction does not link the mechanism crate.
#[cfg(feature = "nip25")]
pub mod nip25;

#[cfg(feature = "nip65")]
pub mod nip65;

// #1239: the retrofit the two comments above anticipated. Each module below is
// a family `nmp-ffi` reached and this facade did not, so a Swift app got it by
// linking one staticlib while a direct-Rust app named a second crate. Each is
// behind its own non-default cargo feature for the same reason `nip22` is: one
// owner of the values, and an app that never uses the family does not link it.
//
// `nip02` is deliberately absent. `nmp-nip02` depends on `nmp` -- it is a
// `protocol-service` in `scripts/dependency-direction-policy.json`, sitting
// ABOVE the facade rather than below it -- so an `nmp -> nmp-nip02` edge is a
// cyclic package dependency cargo refuses to resolve. Reaching the follow
// service through the facade means inverting its engine coupling first, which
// is a different unit of work than this one.
#[cfg(feature = "nip18")]
pub mod nip18;

#[cfg(feature = "nip51")]
pub mod nip51;

#[cfg(feature = "nipc7")]
pub mod nipc7;

#[cfg(feature = "content")]
pub mod content;

#[cfg(feature = "asset")]
pub mod asset;

#[cfg(feature = "blossom")]
pub mod blossom;

// #1033/#824: the app-facing NIP-29 door. A real facade module, not a re-export of
// `nmp-nip29`: the door retains a relay scope AND mints the one opaque
// `WriteIntent`, and a crate that is engine-free by construction cannot do the
// second. `nmp-nip29` stays pure vocabulary below it and this module
// selectively exposes what an app needs of it. The family is optional at the
// native/direct-Rust product boundary just like every other app-facing family.
#[cfg(feature = "nip29")]
pub mod nip29;

pub use auth::{
    AuthPolicy, AuthPolicyDecision, AuthPolicyError, AuthPolicyOp, AuthPolicyPendingSender,
    AuthPolicyRequest, AuthPolicyResolveError, AuthPolicyResult,
};
pub use config::{EngineConfig, DEFAULT_MAX_PUBLISH_ATTEMPTS};
pub use diagnostics::{
    AuthDiagnosticsPhase, AuthDiagnosticsSnapshot, DiagnosticsSnapshot, FilterCoverageEntry,
    RelayDiagnosticsSnapshot, StalledWrite, StalledWriteStage, StalledWriteTotals,
};
pub use engine::RelayInformationRequestError;
pub use engine::{
    AuthPolicyRegistration, CancelWriteError, CancelWriteOutcome, Engine, SignEventRequest,
};
pub use error::EngineError;
pub use observation::ObservationEvidence;
pub use replaceable_materializer::{
    RegisteredReplaceableMaterializer, ReplaceableMaterializer, ReplaceableMaterializerOperation,
    ReplaceableMaterializerRefusal,
};
pub use session::{
    SessionAccount, SessionMutationError, SessionPayload, SessionProvider, SessionRestoreError,
    SessionSnapshot, SigningAvailability,
};

/// Monotonic count of real NMP-owned OS threads spawned this process (#680
/// falsifier instrumentation). The thread-scaling falsifier asserts opening
/// many observations leaves this delta at 0: an observation is a lightweight
/// `Arc`+waker, never an OS thread. Doc-hidden test instrumentation, not part
/// of the public API.
#[doc(hidden)]
#[must_use]
pub fn nmp_threads_spawned() -> u64 {
    nmp_transport::thread_census::nmp_threads_spawned()
}

/// The number of real NMP-owned OS threads currently ALIVE (#704 review
/// falsifier instrumentation). Unlike [`nmp_threads_spawned`] (monotonic), this
/// gauge decrements when a thread exits, so a teardown falsifier can assert it
/// returns to baseline after sessions are dropped and the engine is shut down —
/// proving no orphaned worker survives cancellation/drop/shutdown. Doc-hidden
/// test instrumentation, not part of the public API.
#[doc(hidden)]
#[must_use]
pub fn nmp_threads_live() -> u64 {
    nmp_transport::thread_census::nmp_threads_live()
}
// The pull-based async observation API (#680) is the FFI/SDK delivery
// mechanism — its app contract is documented in `nmp-ffi` and the Swift/Kotlin
// SDKs. The documented direct-Rust public API stays the blocking
// `Subscription`/`recv()` nouns below; these async twins remain fully usable
// (nmp-ffi and any direct-Rust app await them) but are doc-hidden so they do
// not double the facade with generic auto-trait expansions.
#[doc(hidden)]
pub use crate::runtime::ConcurrentNext;
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
    IdentityField, IndexedTagName, LiveQuery, LiveQueryError, Selector, SetAlgebra, SetOp,
    SourceAuthority,
};

// Bech32 nostr-entity DECODE (#116) -- npub/nprofile/note/nevent/naddr ->
// hex id/pubkey + relay hints. A pure codec, unrelated to the two nouns
// above, but "shared, protocol-level" per #116's own framing: a direct-Rust
// app gets it here for the identical reason `nmp-ffi` gets it at the FFI
// boundary, rather than each hand-rolling its own bech32 decode.
pub use nmp_grammar::{decode_nostr_entity, NostrEntity, NostrEntityError};

// The write plane a `WriteIntent` is built from, and its receipt stream.
// `WriteIntent`/`WritePayload`/`WriteRouting` moved to
// `nmp-grammar` (#115 Fable ruling, Fork 3) -- a protocol module composing
// a `WriteIntent` must not gain an engine dependency to do so.
// `WriteRouting` is the whole routing vocabulary and both of its words are
// app-constructible here: `Auto` ("figure out how to route whatever I'm
// publishing") and `Explicit(relays)` ("use these exact relays and that is
// that"). Publishing to a chosen relay is a first-class general capability,
// not a protocol-module privilege -- an app offering "publish this event to
// relay: [user input]" and a crate routing to a group host say the same
// thing the same way.
pub use crate::core::ReceiptId;
pub use crate::publish_queue::{
    AuthDenialSource, NotSentReason, PublishQueueEntry, PublishQueueReadError, ReceiptResult,
    ReceiptResultError, RefuseReason, RelayState, RelayWaiting, RemoveQueueEntryError, RetryCause,
    SigningState, WriteFact, WriteOutcome,
};
pub use crate::runtime::{
    ReceiptReattachment, ReceiptStream, SignEventCancel, SignEventError, SignEventOperation,
};
// The receipt/status receiver is delivery mechanism — it was previously an
// external `std::sync::mpsc::Receiver` (never a documented nmp noun); it is now
// the engine-owned waker-aware FIFO `FifoReceiver` (blocking `recv` for direct
// Rust) plus its async `AsyncFifoReceiver` twin. Both stay doc-hidden so the
// documented public API keeps its previous shape: `publish` returns a
// receipt stream you drain, not a new documented type family.
#[doc(hidden)]
pub use crate::runtime::{
    AsyncFifoReceiver, FifoNextError, FifoReceiver, FifoRecvError, FifoRecvTimeoutError,
    FifoTryRecvError, ReceiptReplayCursor, FACT_CHANNEL_CAPACITY,
};
// Producer-side FIFO mechanism, used only by protocol modules (e.g. nmp-nip02's
// follow-action worker) to feed a receipt/status stream — not app public API,
// so doc-hidden.
#[doc(hidden)]
pub use crate::runtime::{fifo_channel, FifoSender};
// `EventBuilder` collides with `nostr::EventBuilder`, but only INSIDE this
// crate: the re-export below never carried nostr's, and never will, so an
// app sees exactly one `EventBuilder` and never writes a disambiguating
// path. `core` aliases the upstream import where it needs it.
pub use nmp_grammar::{
    CorrelationToken, CorrelationTokenError, EventBuilder, Identity, ReplaceableSourcePolicy,
    WriteIntent, WritePayload, WriteRouting,
};

// #1243: the one tagging door, as an APP uses it. `reply_to` is the general
// reply verb, `text!` writes content whose inline references and rows come
// from one statement, and `Modifiers` is the additive per-relationship
// vocabulary an app spells on a target it is pointing at.
pub use nmp_grammar::{
    reply_to, text, At, InterpolatedContent, Mention, Modifiers, COMMENT_KIND, TEXT_NOTE_KIND,
};
// The implementor half of the same door. `RootScope` is the neutral seam a
// PROTOCOL CRATE implements so an external content id can be a reply target
// without `nmp-grammar` ever naming a NIP, and the rest is the support it
// needs to do so. Every one of those crates depends on `nmp-grammar`
// directly and takes these from there; they are re-exported here only so an
// `nmp`-only consumer can still name the bound on `reply_to`. Doc-hidden for
// the same reason the runtime FIFO family is: the documented public API
// is the two nouns plus the verbs an app calls.
#[doc(hidden)]
pub use nmp_grammar::{
    entity_rows, Pointer, RootScope, TagOptions, TagRows, Tagged, ThreadPosition,
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
// Two distinct coverage scopes live here, deliberately not conflated
// (`docs/design/scoped-evidence-49-12-plan.md` §4): `AcquisitionEvidence`
// (+ `SourceEvidence`/`SourceStatus`/`AuthPhase`/`ShortfallFact`) is the
// scoped, per-query acquisition evidence delivered on every `Frame` --
// per-source facts, never a collapsed completeness verdict.
// `FilterCoverageEntry.coverage` (an `Option<CoverageInterval>`) is the
// engine-global, per-(relay, filter) diagnostics watermark -- unscoped by
// design, and never reused as a query-level verdict either.
pub use crate::core::{
    AcquisitionEvidence, AuthPhase, Row, RowDelta, RowSignature, ShortfallFact, SourceEvidence,
    SourceStatus, WindowLoad,
};
pub use nmp_router::Lane;
pub use nmp_store::CoverageInterval;

// Value types every verb above is expressed in terms of, including the
// `Kind`, `Tag`s and `Timestamp` an app names on an `EventBuilder`.
// `UnsignedEvent` stays because `Handle::sign_event` (#464) takes one:
// signing an event without accepting a write is a door of its own, and its
// argument has to be nameable.
pub use nostr::{Event, EventId, Kind, PublicKey, RelayUrl, Tag, Timestamp, UnsignedEvent};

// Protocol-neutral signer/provider interface. The engine's promotion boundary
// validates every external signer result against the frozen accepted event;
// concrete providers remain optional crates and are not re-exported here.
pub use nmp_signer::{
    CryptoCapability, PendingSignerResolveError, PendingSignerSender, SignerError, SignerOp,
    SignerPublicKey, SignerSignedEvent, SignerUnsignedEvent, SigningCapability,
};

// The concrete mechanism types are internal by default (#52's "internal or
// explicitly unstable"). `Engine::from_parts` needs `PoolConfig` in a
// caller's signature, while `EventStore` remains available to the inner
// core/resolver test seams that are still generic during #1495's contraction.
// Both stay behind the unstable hatch. This is an in-workspace/test exception
// (`nmp-bdd`), not a supported application extension point.
#[cfg(feature = "unstable-mechanism")]
pub use nmp_store::EventStore;
#[cfg(feature = "unstable-mechanism")]
pub use nmp_transport::PoolConfig;
