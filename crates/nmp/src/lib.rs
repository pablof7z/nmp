//! `nmp` -- THE supported Rust product API (#52,
//! #52). Every direct-Rust app and
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
mod subscription;

// The engine is no longer inside this crate. It is two:
//
// - `nmp-engine` — the deterministic reducer. `handle(EngineMsg) ->
//   Vec<Effect>` / `tick(Timestamp) -> Vec<Effect>`, concrete redb I/O, no
//   threads and no sockets.
// - `nmp-runtime` — the async edge that interprets those effects.
//   `EngineThread` (one dedicated OS thread, blocking `mpsc` recv loop, D8)
//   plus the cheap `Clone + Send` `Handle` an app holds, the channels and
//   mailboxes, the AUTH driver, the pool bridge.
//
// The cut is a manifest, and that is the whole reason it is a crate line and
// not a module one. `nmp-engine`'s `Cargo.toml` names no `tokio`, no
// `crossbeam-channel`, no `futures-channel`, no `reqwest`; `nmp-runtime`'s
// names the first two, because interpreting effects is exactly what it is
// for. The reducer's determinism is the foundation of every headless
// falsifier in the workspace, and it used to be protected by a comment in
// this file. It is a build error now.
//
// The `#[doc(hidden)] pub mod mechanism` basement is DELETED with them. It
// existed to let in-workspace harnesses reach `core`/`runtime` while those
// were private modules here, and it did that by publicly re-exporting the
// whole mechanism through a door rustdoc merely hides. `nmp-bdd` and
// `nmp/tests` name `nmp-engine`/`nmp-runtime` directly now, which is both
// honest and narrower: what they reach is those packages' own APIs, not a
// glob of this one's internals. `nmp`'s public API is now exactly the
// re-export list below — visible, and the whole of it.

// #1707 deleted `nip22`/`nip25`/`nip18`/`nipc7`/`content`/`nip65`: each was
// a pure re-export door over its own engine-free mechanism crate -- no
// `WriteIntent`/`Engine`/`Row` construction, nothing the engine needed to
// execute, just vocabulary a caller could equally well reach by naming
// `nmp-nip22`/`nmp-nip25`/`nmp-nip18`/`nmp-nipc7`/`nmp-content`/`nmp-nip65`
// directly.
// `nip65`'s door was `impl Engine { publish_relay_list_bootstrap }`, one
// line of capability convenience (`engine.publish(request.into_write_intent())`)
// wearing an engine-bound signature -- not routing mechanism, so it went
// the same way the others did: deleted, not relocated. `nmp` must not
// contain a single line of any EVENT-KIND CAPABILITY's meaning, re-export
// door or not; a direct-Rust app now names the mechanism crate directly, the
// same way `nmp-ffi`/Swift/Kotlin already do.
//
// Read that word exactly: an event-kind capability is one that owns the
// meaning of some kinds, and the facade names none of them. A protocol
// MECHANISM -- NIP-11 relay information, NIP-42 AUTH --
// is wire/session machinery every app rides whether or not it knows the
// number, has no per-app extension surface, and MAY be named here. Owner
// ruling 2026-08-17, closing #1791, which found this comment and the NIP-11
// re-export below stating exact negations of each other eighty lines apart.
// `docs/internals/crate-architecture.md` rule 2 carries the full statement.
//
// NIP-65's automatic-outbox-discovery ROUTING GLUE used to be the one
// deliberate exception left in this reversal: a feature-gated
// `nmp-runtime::nip65` module, argued for on the grounds that discovering
// an author's relays is the engine's OWN job rather than a capability it
// executes, and that "a second production implementor of author-route
// discovery is what would change that answer".
//
// That is exactly what changed. An outbox algorithm is subjective, and
// other developers must be able to supply their own, so author-route
// discovery is now an adapter seam: `AuthorRouteProvider` (re-exported
// below, declared by `nmp-engine`) is the interface, `nmp-outbox` is the
// NIP-65 implementation of it, and an application picks one at construction
// through `Engine::new_with_capabilities_and_routing` -- the same shape,
// and the same manifest line, as every capability. The feature and the glue
// are deleted; `nmp` names no routing protocol at all.

pub use auth::{
    AuthPolicy, AuthPolicyDecision, AuthPolicyError, AuthPolicyOp, AuthPolicyPendingSender,
    AuthPolicyRequest, AuthPolicyResolveError, AuthPolicyResult,
};
pub use config::EngineConfig;
pub use diagnostics::{
    AuthDiagnosticsSnapshot, DiagnosticsSnapshot, FilterCoverageEntry, RelayDiagnosticsSnapshot,
    StalledWrite, StalledWriteStage, StalledWriteTotals,
};
pub use engine::RelayInformationRequestError;
pub use engine::{
    AuthPolicyRegistration, CancelWriteError, CancelWriteOutcome, Engine, SignEventRequest,
};
pub use error::EngineError;
pub use nmp_grammar::{
    RegisteredReplaceableMaterializer, ReplaceableMaterializer, ReplaceableMaterializerOperation,
    ReplaceableMaterializerRefusal, ReplaceableMaterializerSpec,
};
pub use nmp_runtime::session::{
    SessionAccount, SessionMutationError, SessionPayload, SessionProvider, SessionRestoreError,
    SessionSnapshot, SigningAvailability,
};
pub use observation::ObservationEvidence;

/// Monotonic count of real OS threads NMP spawned through its instrumented
/// paths (#680 falsifier instrumentation). This includes joined engine-owned
/// workers and detached calls into application code such as sign-event
/// completions. Compiled replaceable-capability transformations run directly
/// and do not increment this census. The thread-scaling falsifier asserts
/// opening many observations leaves this delta at 0: an observation is a
/// lightweight `Arc`+waker, never an OS thread. Doc-hidden test
/// instrumentation, not part of the public API.
#[doc(hidden)]
#[must_use]
pub fn nmp_threads_spawned() -> u64 {
    nmp_transport::thread_census::nmp_threads_spawned()
}

/// The number of instrumented OS threads currently ALIVE (#704 review
/// falsifier instrumentation). Unlike [`nmp_threads_spawned`] (monotonic), this
/// gauge decrements when a thread exits. Engine shutdown joins engine-owned
/// workers, but deliberately does not join a detached call into application
/// code such as a sign-event completion; a test must release or finish that
/// foreign callback before expecting the whole-process gauge to return to
/// baseline. Doc-hidden test instrumentation, not part of the public API.
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
pub use nmp_runtime::ConcurrentNext;
// NIP-11 is a protocol MECHANISM, not an event-kind capability (see the
// ruling above), so this re-export is what rule 2 permits rather than an
// exception to it: an app reaches relay information through this facade,
// never as a second Cargo line beside it. The values are `nmp-nip11`'s;
// naming that crate is the engine's business, not the app's. This says
// nothing about the dependency direction -- `nmp-nip11` being a non-optional
// dependency of both `nmp` and `nmp-runtime` is still the workspace's one
// inverted edge, and #1806 owns removing it.
pub use nmp_nip11::{
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
// `selection` is the `Filter`; routing/access/cache are the other axes.
// A branch says one of exactly two things about relays -- the app named
// them, or the app said nothing and NMP routes it -- and nothing infers
// that from the selection's shape (#847). `Demand::public`,
// `Demand::author_outboxes` and `Demand::pinned` are DELETED, along with
// the invented category they named.
pub use nmp_grammar::{
    Binding, CacheMode, Demand, DemandError, Derived, Filter, Freshness,
    IdentityField, IndexedTagName, LiveQuery, LiveQueryError, ReadRouting, Selector, SetAlgebra,
    SetOp,
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
pub use nmp_engine::core::ReceiptId;
pub use nmp_engine::publish_queue::{
    AuthDenialSource, NotSentReason, PublishQueueEntry, PublishQueueReadError, ReceiptResult,
    ReceiptResultError, RefuseReason, RelayState, RelayWaiting, RemoveQueueEntryError, RetryCause,
    SigningState, WriteFact, WriteOutcome, DEFAULT_MAX_PUBLISH_ATTEMPTS,
};
pub use nmp_runtime::{
    ReceiptReattachment, ReceiptStream, SignEventCancel, SignEventError, SignEventOperation,
};
// The receipt/status receiver is delivery mechanism — it was previously an
// external `std::sync::mpsc::Receiver` (never a documented nmp noun); it is now
// the engine-owned waker-aware FIFO `FifoReceiver` (blocking `recv` for direct
// Rust) plus its async `AsyncFifoReceiver` twin. Both stay doc-hidden so the
// documented public API keeps its previous shape: `publish` returns a
// receipt stream you drain, not a new documented type family.
#[doc(hidden)]
pub use nmp_runtime::{
    AsyncFifoReceiver, FifoNextError, FifoReceiver, FifoRecvError, FifoRecvTimeoutError,
    FifoTryRecvError, ReceiptReplayCursor, FACT_CHANNEL_CAPACITY,
};
// Producer-side FIFO mechanism, used only by protocol crates (e.g.
// `nmp-nip02`'s follow observation worker, #1707) to feed a receipt/status
// stream — not app public API, so doc-hidden.
#[doc(hidden)]
pub use nmp_runtime::{fifo_channel, FifoSender};
// `EventBuilder` collides with `nostr::EventBuilder`, but only INSIDE this
// crate: the re-export below never carried nostr's, and never will, so an
// app sees exactly one `EventBuilder` and never writes a disambiguating
// path. `core` aliases the upstream import where it needs it.
pub use nmp_grammar::{EventBuilder, Identity, WriteIntent, WritePayload, WriteRouting};

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
// `AuthDiagnosticsSnapshot`) is facade-OWNED -- defined in
// [`mod@diagnostics`] and exported above, converted once at the
// `DiagnosticsSubscription` delivery boundary -- rather than re-exported
// from the engine (the `bc8fb97` NIP-11 pattern).
//
// Both AUTH phase vocabularies are engine-owned and re-exported here, not
// mirrored (#1616): a facade mirror exists to choose which engine FIELDS an
// app may read, and neither of these closed vocabularies needs a second
// declaration to make that choice. They stay two types because they answer
// two questions. The scoped `AuthPhase` on `SourceStatus::AwaitingAuth`
// carries only the four awaiting members -- an authenticated source is just
// `Requesting` and `AuthDenied` is its own top-level status, so a
// completed/denied "awaiting" would be a representable non-state. The
// engine-global `AuthDiagnosticsPhase` on `DiagnosticsSnapshot.auth_sessions`
// describes one session's whole lifecycle, terminals included, and adds the
// `AwaitingSend` that separates NMP's own pending work from the relay's.
//
// Two distinct coverage scopes live here, deliberately not conflated
// (`docs/design/scoped-evidence-49-12-plan.md` §4): `AcquisitionEvidence`
// (+ `SourceEvidence`/`SourceStatus`/`AuthPhase`/`ShortfallFact`) is the
// scoped, per-query acquisition evidence delivered on every `Frame` --
// per-source facts, never a collapsed completeness verdict.
// `FilterCoverageEntry.coverage` (an `Option<CoverageInterval>`) is the
// engine-global, per-(relay, filter) diagnostics watermark -- unscoped by
// design, and never reused as a query-level verdict either.
pub use nmp_engine::core::{
    AcquisitionEvidence, AuthDiagnosticsPhase, AuthPhase, Row, RowDelta, RowSignature,
    ShortfallFact, SourceEvidence, SourceStatus, WindowLoad,
};

// The construction-time adapter seam for author-route discovery. An app names
// this trait to pass its chosen algorithm to
// `Engine::new_with_capabilities_and_routing`; a crate that IMPLEMENTS one
// names `nmp-engine` and never this facade, which is exactly why nothing here
// mentions NIP-65 or any other routing protocol.
pub use nmp_engine::core::AuthorRouteProvider;
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
// caller's signature. This is an in-workspace/test exception (`nmp-bdd`), not
// a supported application extension point.
#[cfg(feature = "unstable-mechanism")]
pub use nmp_transport::PoolConfig;
