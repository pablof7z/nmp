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

/// One delivered row -- RAW tokens only (ledger #12). Mirrors
/// `nostr::Event`'s wire shape, never a formatted/localized field.
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
}

/// `nmp::RowDelta` mirror -- the wire is deltas, never snapshots (see that
/// type's own doc); the Swift bridge (a later builder) accumulates these
/// into a snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Enum)]
pub enum FfiRowDelta {
    Added { row: FfiRow },
    Removed { id: String },
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

/// `nmp::AuthPhase` mirror -- the AUTH negotiation phases worth surfacing
/// while awaiting proof (reserved for #8; see `nmp_engine::core::evidence`'s
/// own doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub enum FfiAuthPhase {
    AwaitingPolicy,
    AwaitingSignature,
}

/// `nmp::SourceEvidence` mirror -- one relay's acquisition state for a
/// query's subtree, as two deliberately orthogonal facts (see that type's
/// own doc for why `reconciled_through`/`status` must never collapse into
/// one enum).
#[derive(Debug, Clone, PartialEq, Eq, Record)]
pub struct FfiSourceEvidence {
    pub relay: String,
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

/// One delivered batch: raw row deltas + the query's scoped acquisition
/// evidence (mirrors `nmp::RowsMsg`).
#[derive(Debug, Clone, PartialEq, Eq, Record)]
pub struct FfiRowBatch {
    pub deltas: Vec<FfiRowDelta>,
    pub evidence: FfiAcquisitionEvidence,
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
/// form here (#22/#52). `nmp_engine::outbox::NarrowOnly::new`'s
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
    pub wire_sub_count: u32,
    pub authors_served: u32,
    pub by_lane: Vec<FfiLaneCount>,
    /// The EXACT wire JSON of every filter currently sent to this relay
    /// (`ConcreteFilter::to_nostr().as_json()`, rendered engine-side).
    pub filters: Vec<String>,
    pub events_by_kind: Vec<FfiKindCount>,
    pub coverage: Vec<FfiFilterCoverage>,
}

/// The engine-global diagnostics snapshot (M5 plan §1.1) -- "the acceptance
/// test rendered on screen, permanently." Pushed reactively via
/// `NmpEngine::observe_diagnostics`, never polled; read-only and off the
/// data path (never influences routing/delivery).
#[derive(Debug, Clone, PartialEq, Eq, Record)]
pub struct FfiDiagnosticsSnapshot {
    pub relays: Vec<FfiRelayDiagnostics>,
    pub uncovered_author_count: u32,
    pub dropped_merge_rules: Vec<String>,
}

/// The receipt STREAM (`nmp::WriteStatus` mirror; ledger #9 — enqueue is
/// not converged, the app's `ReceiptObserver` may see many of these per
/// publish).
#[derive(Debug, Clone, PartialEq, Eq, Enum)]
pub enum FfiWriteStatus {
    Accepted,
    AwaitingCapability,
    Signed { event_id: String },
    Routed { relays: Vec<String> },
    Sent { relay: String },
    Acked { relay: String },
    Rejected { relay: String, reason: String },
    GaveUp { relay: String },
    PersistenceBlocked { relay: String },
    RoutePersistenceBlocked { relay: String },
    OutcomeUnknown { relay: String },
    Failed { reason: String },
}
