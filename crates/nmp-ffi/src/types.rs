//! The two-noun descriptor value types as UniFFI records/enums (M4 plan §2/
//! §9) -- a thin FFI MIRROR of `nmp_grammar`'s value types, not a re-export.
//! Keeping `nmp-grammar` itself FFI-free (no `uniffi` dependency, no derive
//! macros polluting its `Ord`/`Hash` canonical-hashing contract) is the
//! "cleaner of the two options" the plan calls out in §2 step A; `convert.rs`
//! is the only place that ever bridges between the two.
//!
//! `FfiRow` carries RAW tokens only -- hex pubkey/id/sig, unix timestamp,
//! verbatim tag arrays, verbatim content (VISION ledger #12: no formatted
//! field may ever cross this boundary; `nmp-ffi` has no `display::`
//! anything).

use std::collections::HashMap;
use std::sync::Arc;

use uniffi::{Enum, Record};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub enum FfiRelayInformationCachePolicy {
    UseCache,
    Refresh,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub enum FfiRelayInformationFreshness {
    Fresh,
    Stale,
}

/// Advisory limitation claims understood today. The enclosing document's
/// exact raw JSON remains authoritative for future fields.
#[derive(Debug, Clone, PartialEq, Record)]
pub struct FfiRelayInformationLimitations {
    pub max_message_length: Option<u64>,
    pub max_subscriptions: Option<u64>,
    pub max_filters: Option<u64>,
    pub max_limit: Option<u64>,
    pub max_subid_length: Option<u64>,
    pub max_event_tags: Option<u64>,
    pub max_content_length: Option<u64>,
    pub min_pow_difficulty: Option<u64>,
    pub auth_required: Option<bool>,
    pub payment_required: Option<bool>,
    pub created_at_lower_limit: Option<u64>,
    pub created_at_upper_limit: Option<u64>,
}

/// `nmp::RelayInformationError` mirror (#494) -- typed failure of one
/// bounded NIP-11 acquisition, carried instead of collapsing into a
/// `.to_string()` at either NIP-11 FFI seam (the stale-on-error
/// `FfiRelayInformation.last_error` evidence below, and the acquisition
/// throw in `convert::FfiError::RelayInformationUnavailable`).
#[derive(Debug, Clone, PartialEq, Eq, Enum)]
pub enum FfiRelayInformationErrorKind {
    ExecutorSaturated { capacity: u64 },
    WaiterSaturated { capacity: u64 },
    ThreadUnavailable { reason: String },
    ServiceClosed,
    CredentialedRelayUrl,
    Http { reason: String },
    ResponseTooLarge { limit_bytes: u64 },
    InvalidDocument { reason: String },
}

/// Typed NIP-11 fields understood today. The enclosing record's `raw_json`
/// remains authoritative for fields added by future NIP-11 revisions.
#[derive(Debug, Clone, PartialEq, Record)]
pub struct FfiRelayInformationDocument {
    pub name: Option<String>,
    pub description: Option<String>,
    pub banner: Option<String>,
    pub icon: Option<String>,
    pub pubkey: Option<String>,
    pub self_pubkey: Option<String>,
    pub contact: Option<String>,
    pub supported_nips: Option<Vec<u16>>,
    pub software: Option<String>,
    pub version: Option<String>,
    pub terms_of_service: Option<String>,
    pub limitation: FfiRelayInformationLimitations,
    pub structured: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Record)]
pub struct FfiRelayInformation {
    pub relay: String,
    pub document: FfiRelayInformationDocument,
    pub raw_json: String,
    pub document_revision: String,
    pub fetched_at: u64,
    pub fresh_until: u64,
    pub freshness: FfiRelayInformationFreshness,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub cache_control: Option<String>,
    pub expires: Option<String>,
    pub last_error: Option<FfiRelayInformationErrorKind>,
}

/// The reactive identity root (VISION §2 P3). Extensible -- UniFFI enums are
/// NOT `#[non_exhaustive]` across the FFI boundary by default, but adding a
/// variant here is a mechanical, additive change on both sides whenever the
/// grammar itself grows one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub enum FfiIdentityField {
    ActivePubkey,
}

/// The closed projection vocabulary (`nmp_grammar::Selector` mirror).
#[derive(Debug, Clone, PartialEq, Eq, Enum)]
pub enum FfiSelector {
    Authors,
    Ids,
    /// `name` is an arbitrary event-tag key (#64) -- a purely local
    /// projection over already-acquired events, NOT restricted to
    /// `FfiFilter.tags`' single-letter wire-filter alphabet. Passed through
    /// unchanged by `convert::selector_from_ffi`: `"-"`, `"poop"`, `"alt"`,
    /// or any other multi-character/punctuation tag name an event actually
    /// carries is a legal key here.
    Tag {
        name: String,
    },
    AddressCoord,
}

/// Set algebra over resolved value sets (`nmp_grammar::SetAlgebra` mirror).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub enum FfiSetAlgebra {
    Union,
    Intersect,
    Diff,
}

/// Every bindable filter-field value (`nmp_grammar::Binding` mirror).
/// Recursive through `FfiDerived`/`FfiSetOp` -- both are UniFFI **objects**
/// (`Arc`-handles), not records: UniFFI's proc-macro mode lowers/lifts
/// `Arc<T>` only when `T` is itself an exported object (an opaque handle),
/// never a plain `Box<T>`/`Arc<T>`-wrapped record -- and a record directly
/// containing itself is a compile-time infinite-size error in Rust regardless
/// of UniFFI. Making the recursive point an object (constructor + getters,
/// see each type's `impl`) is the standard UniFFI idiom for a recursive value
/// type; it mirrors `nmp_grammar::Binding`'s own `Box<Derived>`/`Box<SetOp>`
/// indirection, just via an opaque handle instead of a boxed value. `SetOp`'s
/// `operands: Vec<FfiBinding>` needs no such indirection -- `Vec` is already
/// heap-allocated, breaking the cycle on its own.
#[derive(Debug, Clone, PartialEq, Eq, Enum)]
pub enum FfiBinding {
    Literal { values: Vec<String> },
    Reactive { field: FfiIdentityField },
    Derived { derived: Arc<FfiDerived> },
    SetOp { set_op: Arc<FfiSetOp> },
}

/// A `Binding::Derived` payload mirror -- a UniFFI object (see [`FfiBinding`]'s
/// doc for why): Swift constructs one via `FfiDerived(inner:project:)` and
/// reads it back via the `inner()`/`project()` getters.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Object)]
pub struct FfiDerived {
    pub inner: FfiFilter,
    pub project: FfiSelector,
}

#[uniffi::export]
impl FfiDerived {
    #[uniffi::constructor]
    pub fn new(inner: FfiFilter, project: FfiSelector) -> Arc<Self> {
        Arc::new(Self { inner, project })
    }

    pub fn inner(&self) -> FfiFilter {
        self.inner.clone()
    }

    pub fn project(&self) -> FfiSelector {
        self.project.clone()
    }
}

/// A `Binding::SetOp` payload mirror -- a UniFFI object, same reasoning as
/// [`FfiDerived`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Object)]
pub struct FfiSetOp {
    pub op: FfiSetAlgebra,
    pub operands: Vec<FfiBinding>,
}

#[uniffi::export]
impl FfiSetOp {
    #[uniffi::constructor]
    pub fn new(op: FfiSetAlgebra, operands: Vec<FfiBinding>) -> Arc<Self> {
        Arc::new(Self { op, operands })
    }

    pub fn op(&self) -> FfiSetAlgebra {
        self.op
    }

    pub fn operands(&self) -> Vec<FfiBinding> {
        self.operands.clone()
    }
}

/// A live-query filter whose field values may be [`FfiBinding`]s
/// (`nmp_grammar::Filter` mirror). `tags` is keyed by the tag's single
/// character as a one-character `String` (UniFFI has no native `char`
/// mirror as clean as this one) -- `convert::indexed_tag_name_from_ffi`
/// validates every key is exactly one ASCII letter (`a`-`z`/`A`-`Z`, all 52
/// valid) on the way in. This is the wire/local INDEXED filter alphabet
/// only (NIP-01 `#<letter>` queries) -- it is a distinct concept from
/// [`FfiSelector::Tag`]'s arbitrary event-tag key, which is never restricted
/// to a single letter.
#[derive(Debug, Clone, PartialEq, Eq, Default, Record)]
pub struct FfiFilter {
    pub kinds: Option<Vec<u16>>,
    pub authors: Option<FfiBinding>,
    pub ids: Option<FfiBinding>,
    pub tags: HashMap<String, FfiBinding>,
    pub since: Option<u64>,
    pub until: Option<u64>,
    pub limit: Option<u32>,
}

/// Which authority resolves a [`FfiDemand`]'s relay set
/// (`nmp_grammar::SourceAuthority` mirror, #107). `relays` is a raw URL
/// string list -- `convert::demand_from_ffi` parses/canonicalizes/
/// dedupes/sorts each one and rejects an empty set with a typed
/// [`crate::convert::FfiError`], never a panic.
#[derive(Debug, Clone, PartialEq, Eq, Enum)]
pub enum FfiSourceAuthority {
    AuthorOutboxes,
    Public,
    Pinned { relays: Vec<String> },
}

/// `nmp_grammar::AccessContext` mirror with a stable expected NIP-42 key.
#[derive(Debug, Clone, PartialEq, Eq, Enum)]
pub enum FfiAccessContext {
    Public,
    Nip42 { public_key: String },
}

/// `nmp_grammar::CacheMode` mirror (#107). Meaningful only alongside
/// `FfiSourceAuthority::Pinned` -- see that type's doc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub enum FfiCacheMode {
    Agnostic,
    Strict,
}

/// The full live-query identity an app declares -- `selection + source +
/// access + cache` (`nmp_grammar::Demand` mirror, #106/#107). `NmpEngine::
/// observe` still accepts a bare [`FfiFilter`] for the common case (the
/// static `AuthorOutboxes`/`Public` default, #106's `Demand::from_filter`);
/// this is the explicit constructor an app reaches for once it needs to
/// declare `Pinned` wire authority or a non-`Agnostic` cache mode.
#[derive(Debug, Clone, PartialEq, Eq, Record)]
pub struct FfiDemand {
    pub selection: FfiFilter,
    pub source: FfiSourceAuthority,
    pub access: FfiAccessContext,
    pub cache: FfiCacheMode,
}

/// Window policy on the read noun (#485, `nmp::Window` mirror). One real
/// variant today; future policies (latest/anchored) are new VARIANTS of this
/// enum, never new nouns or parallel observe verbs. `initial`/`max` are row
/// counts -- `convert::window_from_ffi` rejects zeroes and `initial > max`
/// with a typed [`crate::convert::FfiError`], never a panic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub enum FfiWindow {
    /// Bounded newest-first window: starts with `initial` canonical rows,
    /// grows only by explicit `NmpQueryHandle::request_rows`, never above
    /// `max`.
    Expandable { initial: u64, max: u64 },
}

/// The complete current bounded row set of a windowed observation, plus its
/// mechanical growth fact. Rows are canonical newest-first
/// (`created_at DESC, event_id ASC`); the native bridge REPLACES its row
/// state from `rows` wholesale -- it never folds deltas for windowed frames.
#[derive(Debug, Clone, PartialEq, Eq, Record)]
pub struct FfiWindowContents {
    pub rows: Vec<FfiRow>,
    pub load: FfiWindowLoad,
}

/// Mechanical growth state of an expandable window (`nmp::WindowLoad`
/// mirror). Deliberately no Complete/End/Synced variant: `Returned { added:
/// 0 }` only means the planned advance added no canonical row -- consult the
/// frame's per-source acquisition evidence for why, never a global verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub enum FfiWindowLoad {
    Idle,
    Requesting,
    Returned { added: u64 },
    AtBound { max: u64 },
}

/// One delivered observation frame (`nmp::Frame` mirror) -- the ONE
/// vocabulary both observation modes share. Delivery is DERIVED from
/// boundedness, never a knob, and never carried twice on the wire:
///
/// - Unbounded (`window` is `None`): `deltas` is the exact lossless
///   transition from the previously delivered frame; the full row set is
///   never redelivered (full-set redelivery is the O(rows squared) P0 #485
///   exists to kill).
/// - Windowed (`window` is `Some`): `window.rows` is the complete current
///   bounded set and `deltas` is ALWAYS empty -- bridges replace state from
///   the snapshot, so shipping deltas too would cross every row the FFI
///   boundary twice just to be folded and discarded.
#[derive(Debug, Clone, PartialEq, Eq, Record)]
pub struct FfiFrame {
    /// Unbounded observations: the exact delta transition. ALWAYS empty for
    /// windowed observations (see the type doc).
    pub deltas: Vec<FfiRowDelta>,
    /// Present iff the observation is windowed: the complete bounded row set
    /// plus the window's growth fact.
    pub window: Option<FfiWindowContents>,
    pub evidence: FfiAcquisitionEvidence,
}

/// One delivered row -- RAW tokens only (ledger #12). Mirrors
/// `nostr::Event`'s wire shape, never a formatted/localized field, plus
/// `nmp::Row::sources` (#105): the sorted, deduplicated relay-observation
/// set for this exact event id -- not a formatted/display field either,
/// just the raw relay URLs that have delivered it.
#[derive(Debug, Clone, PartialEq, Eq, Record)]
pub struct FfiRow {
    pub id: String,
    pub pubkey: String,
    pub created_at: u64,
    pub kind: u16,
    /// Each inner `Vec<String>` is one raw tag array (`["p", "<hex>", ...]`),
    /// verbatim -- never parsed into a display-facing shape here.
    pub tags: Vec<Vec<String>>,
    pub content: String,
    pub sig: String,
    /// Sorted, deduplicated relay URLs that have delivered this event id.
    pub sources: Vec<String>,
}

/// Immutable NIP-01 event body accepted by the governed sign-only operation.
/// The author is deliberately absent and is frozen from engine identity state.
#[derive(Debug, Clone, PartialEq, Eq, Record)]
pub struct FfiSignEventRequest {
    pub created_at: u64,
    pub kind: u16,
    pub tags: Vec<Vec<String>>,
    pub content: String,
}

/// Exact verified result of a sign-only operation. This is an event value,
/// not a canonical store row: it has no relay provenance and was not
/// published or persisted by signing.
#[derive(Debug, Clone, PartialEq, Eq, Record)]
pub struct FfiSignedEvent {
    pub id: String,
    pub pubkey: String,
    pub created_at: u64,
    pub kind: u16,
    pub tags: Vec<Vec<String>>,
    pub content: String,
    pub sig: String,
}

/// Failures that may resolve after a sign-only operation was accepted.
#[derive(Debug, Clone, PartialEq, Eq, Enum)]
pub enum FfiSignEventFailure {
    SignerUnavailable { reason: String },
    SignerRejected { reason: String },
    InvalidSignerOutput { reason: String },
    Cancelled,
}

/// A remembered NIP-29 group reference (#108, `nmp_nip29::GroupRef`
/// mirror) -- group id, host relay, and optional display name.
#[derive(Debug, Clone, PartialEq, Eq, Record)]
pub struct FfiGroupRef {
    pub group_id: String,
    pub host: String,
    pub name: Option<String>,
}

/// The composed remembered-groups/host-relays value (#108,
/// `nmp_nip29::RememberedGroups` mirror) -- what
/// `crate::nip29::decode_remembered_groups` returns from a delivered
/// kind:10009 [`FfiRow`].
#[derive(Debug, Clone, PartialEq, Eq, Record)]
pub struct FfiRememberedGroups {
    pub groups: Vec<FfiGroupRef>,
    pub hosts_in_use: Vec<String>,
    pub has_private_content: bool,
}

/// `nmp::RowDelta` mirror. For UNBOUNDED observations the wire is deltas,
/// never snapshots (see that type's own doc); the native bridge accumulates
/// these into a snapshot. Windowed observations instead deliver the whole
/// bounded set in [`FfiFrame::window`] and carry an empty delta list --
/// delivery mode derives from boundedness, never both at once.
#[derive(Debug, Clone, PartialEq, Eq, Enum)]
pub enum FfiRowDelta {
    Added {
        row: FfiRow,
    },
    /// #105: the SAME row already matched; its relay-provenance set grew.
    /// Carries the FULL current source set (matching `Added`'s own
    /// "whole value, not a patch" shape), never the event body again.
    SourcesGrew {
        id: String,
        sources: Vec<String>,
    },
    Removed {
        id: String,
    },
}

/// `nmp::SourceStatus` mirror (`docs/design/scoped-evidence-49-12-plan.md`
/// §4) -- the closed, honest per-source link-status vocabulary for the
/// scoped, per-query [`FfiAcquisitionEvidence`] surface. Ratified names,
/// codex-nova-governed: no variant/field may be added beyond this list, and
/// no query-level aggregate may ever be added anywhere on
/// this surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub enum FfiSourceStatus {
    Requesting,
    Connecting,
    Disconnected,
    AwaitingAuth { phase: FfiAuthPhase },
    AuthDenied,
    Error,
}

/// Closed AUTH phase vocabulary shared by scoped acquisition evidence and
/// engine-global AUTH diagnostics. Scoped evidence uses only the awaiting
/// variants; completed/denied/error truth remains top-level in
/// [`FfiSourceStatus`] and appears here only for a diagnostics session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub enum FfiAuthPhase {
    AwaitingChallenge,
    AwaitingPolicy,
    AwaitingSignature,
    AwaitingRelayAck,
    Ready,
    Denied,
    Error,
}

/// `nmp::SourceEvidence` mirror -- one relay's acquisition state for a
/// query's subtree, as two deliberately orthogonal facts (see that type's
/// own doc for why `reconciled_through`/`status` must never collapse into
/// one enum).
#[derive(Debug, Clone, PartialEq, Eq, Record)]
pub struct FfiSourceEvidence {
    pub relay: String,
    pub access: FfiAccessContext,
    pub reconciled_through: Option<u64>,
    pub status: FfiSourceStatus,
}

/// `nmp::ShortfallFact` mirror -- an explicit, never-silent shortfall in a
/// query's subtree acquisition (never folded into `sources`).
#[derive(Debug, Clone, PartialEq, Eq, Enum)]
pub enum FfiShortfallFact {
    NoPlannedSource { atom: String },
    NoResolvedDemand,
    LocalLimit { atom: String },
}

/// `nmp::AcquisitionEvidence` mirror (`docs/design/scoped-evidence-49-12-plan.md`
/// §4, folding #12 into #49) -- per-SOURCE facts for a query's full subtree
/// (interior `Derived` atoms included), plus an explicit shortfall list.
/// Replaces the deleted query-level aggregate: NO field here is, or may ever
/// become, a global verdict -- an app rolls per-source facts into its own
/// progress policy, NMP never does that rollup for it.
#[derive(Debug, Clone, PartialEq, Eq, Record)]
pub struct FfiAcquisitionEvidence {
    pub sources: Vec<FfiSourceEvidence>,
    pub shortfall: Vec<FfiShortfallFact>,
}

/// `nmp::Durability` mirror (a typed PROPERTY of a write, not a routing
/// choice -- M0 amendment).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub enum FfiDurability {
    Durable,
    Ephemeral,
    AtMostOnce,
}

/// `nmp::WriteRouting` mirror -- `PrivateNarrow` deliberately has NO wire
/// form here (#22/#52). `nmp_grammar::NarrowOnly::new`'s
/// own doc requires "the caller must already have resolved and narrowed
/// this itself" -- i.e. a trusted protocol module's own logic, not a raw
/// relay-URL string handed across the FFI boundary by an app with no way to
/// prove those URLs are actually private. Minting `PrivateRoute` from
/// FFI-supplied strings would be exactly the "raw app-provided expanded
/// relay set"/"route escape hatch" #22's canonical design rules out; the
/// `nmp` facade itself withholds re-exporting `NarrowOnly`/`PrivateRoute`
/// for the identical reason (see `crates/nmp/src/lib.rs`'s doc). A
/// validated, opaque private-route mint belongs in a protocol module built
/// on direct Rust, not this FFI surface -- `AuthorOutbox`/`ToInboxes`
/// remain the only FFI-constructible routing choices for now.
///
/// #115: `PinnedHost(HostAuthority)` gets the IDENTICAL treatment, for the
/// IDENTICAL reason -- this enum deliberately gains NO variant for it, and
/// no `HostAuthority` constructor is exported anywhere in this crate. An
/// app can only ever obtain a pinned-host write transitively, through a
/// protocol module's already-composed intent (`NmpEngine::group_message_intent`
/// -> the opaque `FfiComposedWriteIntent`) -- never by naming a host
/// itself. This exhaustive two-variant match IS the enforcement: a new
/// `WriteRouting` variant landing in `nmp-grammar` without a corresponding
/// FfiWriteRouting decision is a compile error here, not a silent gap.
#[derive(Debug, Clone, PartialEq, Eq, Enum)]
pub enum FfiWriteRouting {
    AuthorOutbox,
    ToInboxes { recipients: Vec<String> },
}

/// The event payload of a write intent (`nmp::WritePayload` mirror). VISION
/// P: signing and publishing are ORTHOGONAL stages -- `Unsigned` is a
/// template the engine signs internally ("the key lives in the engine",
/// ledger #12); `Signed` (#32, the M5 unlock) is a caller that already
/// holds a validly-signed event -- an external signer / NIP-46 bunker, or a
/// verbatim republish -- and hands its fields across as-is. `Signed`'s
/// fields are field-for-field [`FfiRow`] (the read-side mirror of a signed
/// `nostr::Event`) plus `sig`, deliberately: the write side stays symmetric
/// with the read side rather than introducing a JSON-blob shape.
///
/// `Signed`'s fields are PARSED at this FFI boundary (typed hex/signature-
/// shape errors, see `convert::signed_event_from_ffi`) but NOT verified
/// here (#52 Unit B) -- `nostr::Event::verify` runs at
/// `nmp-engine::core::EngineCore::on_publish`'s acceptance boundary (Unit
/// A0/#56) instead, so the guarantee holds for every entry point, not only
/// this one. A tampered `Signed` event still parses fine here and is
/// rejected downstream, surfacing as `WriteStatus::Failed` on the receipt
/// stream rather than a synchronous `FfiError`. The engine itself never
/// re-signs, mutates a tag, or recomputes an id for this variant.
#[derive(Debug, Clone, PartialEq, Eq, Enum)]
pub enum FfiWritePayload {
    Unsigned {
        pubkey: String,
        created_at: u64,
        kind: u16,
        tags: Vec<Vec<String>>,
        content: String,
    },
    Signed {
        id: String,
        pubkey: String,
        created_at: u64,
        kind: u16,
        tags: Vec<Vec<String>>,
        content: String,
        sig: String,
    },
}

/// A caller's publish request (`nmp::WriteIntent` mirror).
#[derive(Debug, Clone, PartialEq, Eq, Record)]
pub struct FfiWriteIntent {
    pub payload: FfiWritePayload,
    pub durability: FfiDurability,
    pub routing: FfiWriteRouting,
}

/// One (relay, kind) event count -- `nmp::DiagnosticsSnapshot`'s
/// events-received-per-kind mirror (M5 plan §1.1): the one datum
/// `nmp-router`'s own `Diagnostics` cannot see, since it only ever reflects
/// what was compiled/sent, never what was actually received.
#[derive(Debug, Clone, PartialEq, Eq, Record)]
pub struct FfiKindCount {
    pub kind: u16,
    pub count: u64,
}

/// One lane's wire-req count within a relay's diagnostics (M5 plan §1.1;
/// `nmp::Lane` mirror, rendered as a string -- see
/// `convert::lane_to_ffi_string`).
#[derive(Debug, Clone, PartialEq, Eq, Record)]
pub struct FfiLaneCount {
    pub lane: String,
    pub count: u32,
}

/// `nmp::CoverageInterval` mirror -- a proven, retained `[from, through]`
/// interval (`nmp_store::coverage::CoverageInterval`). This is the
/// engine-global DIAGNOSTICS watermark, deliberately distinct from the
/// scoped, per-query [`FfiAcquisitionEvidence`] surface above (M5 plan §1
/// vs. `docs/design/scoped-evidence-49-12-plan.md` §4) -- never reused as a
/// query-level verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Record)]
pub struct FfiCoverageInterval {
    pub from: u64,
    pub through: u64,
}

/// One filter's proven coverage state at one relay (M5 plan §1.1). `filter`
/// is the EXACT wire JSON -- the same rendering as the parallel entry in
/// `FfiRelayDiagnostics.filters`. `coverage` is `None` -- "no row = not
/// covered", unchanged from the store's own rule.
#[derive(Debug, Clone, PartialEq, Eq, Record)]
pub struct FfiFilterCoverage {
    pub filter: String,
    pub coverage: Option<FfiCoverageInterval>,
}

/// One relay's full diagnostics (M5 plan §1.1) -- per-relay wire-sub count,
/// exact filters, lane breakdown, reverse coverage (authors served), events
/// actually received per kind, and per-filter coverage state. Every field
/// here is a REAL number read off the running engine -- never fabricated or
/// estimated (the plan's truth-anchor rule).
#[derive(Debug, Clone, PartialEq, Eq, Record)]
pub struct FfiRelayDiagnostics {
    pub relay: String,
    pub access: FfiAccessContext,
    pub wire_sub_count: u32,
    pub authors_served: u32,
    pub by_lane: Vec<FfiLaneCount>,
    /// The EXACT wire JSON of every filter currently sent to this relay
    /// (`ConcreteFilter::to_nostr().as_json()`, rendered engine-side).
    pub filters: Vec<String>,
    pub events_by_kind: Vec<FfiKindCount>,
    pub coverage: Vec<FfiFilterCoverage>,
    pub nip11_supported_nips: Option<Vec<u16>>,
    pub nip11_document_revision: Option<String>,
    pub nip11_freshness: Option<String>,
    pub nip11_last_error: Option<String>,
    pub nip77_advertisement: String,
    pub nip77_behavior: String,
}

/// One bounded exact-session AUTH diagnostics record. `relay + access`
/// identifies the session. Capability-instance ids and the raw challenge do
/// not cross FFI: only binding booleans and the engine's BLAKE3 challenge
/// descriptor are exposed. `AwaitingRelayAck` covers the post-signature
/// send/ack span; `send_handoff_accepted` distinguishes whether transport
/// accepted the AUTH event yet.
#[derive(Debug, Clone, PartialEq, Eq, Record)]
pub struct FfiAuthDiagnostics {
    pub relay: String,
    pub access: FfiAccessContext,
    pub transport_generation: u64,
    pub epoch_sequence: Option<u64>,
    pub challenge_descriptor: Option<String>,
    pub phase: FfiAuthPhase,
    pub policy_bound: bool,
    pub signer_bound: bool,
    pub auth_event_id: Option<String>,
    pub send_handoff_accepted: bool,
    pub relay_ok_accepted: bool,
}

/// The engine-global diagnostics snapshot (M5 plan §1.1) -- "the acceptance
/// test rendered on screen, permanently." Pushed reactively via
/// `NmpEngine::observe_diagnostics`, never polled; read-only and off the
/// data path (never influences routing/delivery).
#[derive(Debug, Clone, PartialEq, Eq, Record)]
pub struct FfiDiagnosticsSnapshot {
    pub relays: Vec<FfiRelayDiagnostics>,
    /// At most one record per currently connected protected session.
    pub auth_sessions: Vec<FfiAuthDiagnostics>,
    pub uncovered_author_count: u32,
    pub dropped_merge_rules: Vec<String>,
    /// DISCOVERED relays rejected by the SSRF admission policy before they
    /// could become routable lanes (issue #121): a network-sourced kind:10002
    /// naming a loopback/RFC-1918/link-local/`.onion` host the operator never
    /// opted in. Counted PER LANE (write + read parse of one event), so a
    /// single hostile event naming `N` rejected hosts bumps this by up to
    /// `2N` — a rejection-event tally, not a distinct-host count.
    pub discovered_private_relays_rejected: u64,
    /// Session dials the transport pool refused because the configured
    /// `max_relays` ceiling was already reached (issue #121, worker-exhaustion
    /// defense). Always `0` when no cap is configured.
    pub sessions_rejected_over_cap: u64,
    /// Latest transport acceptance/verifier failure, if any. This is
    /// observational diagnostics and never changes routing or trust policy.
    pub transport_degraded: Option<String>,
}

/// The receipt STREAM (`nmp::WriteStatus` mirror; ledger #9 — enqueue is
/// not converged, the app's `ReceiptObserver` may see many of these per
/// publish).
#[derive(Debug, Clone, PartialEq, Eq, Enum)]
pub enum FfiWriteStatus {
    Accepted,
    AwaitingCapability,
    Signed {
        event_id: String,
    },
    Routed {
        relays: Vec<String>,
    },
    AwaitingRelay {
        relay: String,
    },
    AwaitingAuth {
        relay: String,
    },
    RetryEligible {
        relay: String,
        attempt: u64,
        eligible_at: u64,
    },
    HandoffAmbiguous {
        relay: String,
        attempt: u64,
        observed_at: u64,
    },
    Sent {
        relay: String,
        attempt: u64,
        written_at: u64,
    },
    Acked {
        relay: String,
    },
    Rejected {
        relay: String,
        reason: String,
    },
    GaveUp {
        relay: String,
    },
    PersistenceBlocked {
        relay: String,
    },
    RoutePersistenceBlocked {
        relay: String,
    },
    OutcomeUnknown {
        relay: String,
    },
    ReplaceableConflict {
        expected: Option<String>,
        actual: Option<String>,
    },
    Failed {
        reason: String,
    },
}

/// Result of looking up a stable retained receipt id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub enum FfiReceiptReattachment {
    Attached,
    NotFound,
    RetainedButUnreadable,
}

/// A decoded public NIP-19 nostr entity (#116, `nmp::NostrEntity` mirror).
/// Each variant carries EXACTLY the fields NIP-19 defines for that entity --
/// never force-fit into one shared shape: `npub`/`note` carry no relay
/// hints at all (the format has none to carry); `nevent`'s `author`/`kind`
/// are independently optional metadata; `naddr`'s `kind`/`author`/
/// `identifier` are ALL required by the format, unlike `nevent`'s. There is
/// deliberately no `nsec`/`ncryptsec` variant here -- see
/// `convert::decode_nostr_entity`'s doc for why a secret-key entity is
/// refused rather than decoded.
#[derive(Debug, Clone, PartialEq, Eq, Enum)]
pub enum FfiNostrEntity {
    Pubkey {
        pubkey: String,
    },
    Profile {
        pubkey: String,
        relays: Vec<String>,
    },
    EventId {
        id: String,
    },
    Event {
        id: String,
        author: Option<String>,
        kind: Option<u16>,
        relays: Vec<String>,
    },
    Coordinate {
        kind: u16,
        author: String,
        identifier: String,
        relays: Vec<String>,
    },
}
