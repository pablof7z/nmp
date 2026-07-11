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
    /// `name` is exactly one character from the closed M1 tag-name set
    /// (`p, e, a, d, E, t, q`) -- validated on the way IN by
    /// `convert::tag_name_from_ffi`, never trusted verbatim.
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
/// mirror as clean as this one) -- `convert::tag_name_from_ffi` validates
/// every key against the closed M1 set on the way in.
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

/// `nmp_engine::core::RowDelta` mirror -- the wire is deltas, never
/// snapshots (see that type's own doc); the Swift bridge (a later builder)
/// accumulates these into a snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Enum)]
pub enum FfiRowDelta {
    Added { row: FfiRow },
    Removed { id: String },
}

/// `nmp_engine::core::QueryCoverage` mirror (ruling §6 — ledger #7's
/// variant).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub enum FfiCoverage {
    CompleteUpTo { unix_seconds: u64 },
    Unknown,
}

/// One delivered batch: raw row deltas + the query's aggregate coverage
/// (mirrors `nmp_engine::runtime::RowsMsg`).
#[derive(Debug, Clone, PartialEq, Eq, Record)]
pub struct FfiRowBatch {
    pub deltas: Vec<FfiRowDelta>,
    pub coverage: FfiCoverage,
}

/// `nmp_engine::outbox::Durability` mirror (a typed PROPERTY of a write, not
/// a routing choice -- M0 amendment).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub enum FfiDurability {
    Durable,
    Ephemeral,
    AtMostOnce,
}

/// `nmp_engine::outbox::WriteRouting` mirror. `PrivateNarrow`'s `relays` is
/// the fixed, fail-closed set itself (ledger #6) -- an empty `Vec` here is
/// exactly how "unroutable" is expressed; there is no widen operation on
/// the wire, matching `NarrowOnly`'s own construction discipline.
#[derive(Debug, Clone, PartialEq, Eq, Enum)]
pub enum FfiWriteRouting {
    AuthorOutbox,
    ToInboxes { recipients: Vec<String> },
    PrivateNarrow { relays: Vec<String> },
}

/// A caller's publish request (`nmp_engine::outbox::WriteIntent` mirror).
/// M4 scope note: the payload is ALWAYS an unsigned template -- "the key
/// lives in the engine" (VISION ledger #12; M4 plan §5's `addAccount`
/// framing) means an app never holds a signed event to hand across this
/// boundary in M4; a `Signed`-payload variant (republishing an
/// already-signed event to a recomputed relay set) is an explicit
/// non-goal-for-now, not an oversight -- see the plan's §10 non-goals list
/// for the analogous remote-signer deferral.
#[derive(Debug, Clone, PartialEq, Eq, Record)]
pub struct FfiWriteIntent {
    pub pubkey: String,
    pub created_at: u64,
    pub kind: u16,
    pub tags: Vec<Vec<String>>,
    pub content: String,
    pub durability: FfiDurability,
    pub routing: FfiWriteRouting,
}

/// One (relay, kind) event count -- `nmp_engine::core::DiagnosticsSnapshot`'s
/// events-received-per-kind mirror (M5 plan §1.1): the one datum
/// `nmp-router`'s own `Diagnostics` cannot see, since it only ever reflects
/// what was compiled/sent, never what was actually received.
#[derive(Debug, Clone, PartialEq, Eq, Record)]
pub struct FfiKindCount {
    pub kind: u16,
    pub count: u64,
}

/// One lane's wire-req count within a relay's diagnostics (M5 plan §1.1;
/// `nmp_router::Lane` mirror, rendered as a string -- see
/// `convert::lane_to_ffi_string`).
#[derive(Debug, Clone, PartialEq, Eq, Record)]
pub struct FfiLaneCount {
    pub lane: String,
    pub count: u32,
}

/// One filter's proven coverage state at one relay (M5 plan §1.1). `filter`
/// is the EXACT wire JSON -- the same rendering as the parallel entry in
/// `FfiRelayDiagnostics.filters`.
#[derive(Debug, Clone, PartialEq, Eq, Record)]
pub struct FfiFilterCoverage {
    pub filter: String,
    pub coverage: FfiCoverage,
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

/// The receipt STREAM (`nmp_engine::outbox::WriteStatus` mirror; ledger #9 —
/// enqueue is not converged, the app's `ReceiptObserver` may see many of
/// these per publish).
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
    Failed { reason: String },
}
