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
    // #704: `WaiterSaturated`/`ThreadUnavailable` were removed -- the async
    // NIP-11 fetch has no waiter/thread admission refusal to report.
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
/// reads it back via the `inner()`/`project()` getters. `inner` is a complete
/// [`FfiDemand`], matching Rust's `nmp_grammar::Derived`: a nested query owns
/// its source, access, cache, and freshness policy independently from the
/// outer demand (#714).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Object)]
pub struct FfiDerived {
    pub inner: FfiDemand,
    pub project: FfiSelector,
}

#[uniffi::export]
impl FfiDerived {
    #[uniffi::constructor]
    pub fn new(inner: FfiDemand, project: FfiSelector) -> Arc<Self> {
        Arc::new(Self { inner, project })
    }

    pub fn inner(&self) -> FfiDemand {
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

/// `nmp_grammar::Freshness` mirror (#565). Whole seconds are the exact
/// precision of Nostr timestamps and persisted coverage watermarks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub enum FfiFreshness {
    Live,
    MaxAge { seconds: u64 },
    CacheOnly,
}

/// The full live-query declaration an app supplies -- `selection + source +
/// access + cache + freshness` (`nmp_grammar::Demand` mirror, #106/#107/#565). `NmpEngine::
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
    pub freshness: FfiFreshness,
}

/// Window policy on the read noun (#485, `nmp::Window` mirror). One real
/// variant today; future policies (latest/anchored) are new VARIANTS of this
/// enum, never new nouns or parallel observe verbs. `initial`/`max` are row
/// counts -- `convert::window_from_ffi` rejects zeroes and `initial > max`
/// with a typed [`crate::convert::FfiError`], never a panic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub enum FfiWindow {
    /// Bounded newest-first window: starts with `initial` canonical rows,
    /// grows only by explicit `NmpRowStream::request_rows`, never above
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

/// One live-query declaration (`nmp::LiveQuery` mirror, #1108): one or more
/// complete, independent [`FfiDemand`] branches observed through ONE
/// lifecycle, plus the optional bound on their merged row union.
///
/// Branches are canonicalized by the Rust constructor -- sorted, duplicates
/// collapsed -- so permuted or repeated input produces the same observation
/// and the same per-branch evidence order. `aggregate_result_limit` bounds
/// the union AFTER branch rows are merged by event id; it is NOT a branch's
/// own `FfiFilter::limit`, which bounds only that branch's selection.
#[derive(Debug, Clone, PartialEq, Eq, Record)]
pub struct FfiLiveQuery {
    /// The demand branches. Must be nonempty and at most
    /// `MAX_QUERY_BRANCHES`; both are typed refusals, never truncation.
    pub branches: Vec<FfiDemand>,
    /// Bound on the MERGED row union, never N rows per branch. Zero is a
    /// typed refusal.
    pub aggregate_result_limit: Option<u32>,
}

/// The hard ceiling on branches in one observation (`nmp::LiveQuery::
/// MAX_BRANCHES` mirror). Exceeding it refuses the whole declaration.
#[uniffi::export]
pub fn max_query_branches() -> u32 {
    nmp::LiveQuery::MAX_BRANCHES as u32
}

/// `nmp::LiveQuery::union` as a free function, so a hand-written Swift or
/// Kotlin live-query value can be built by the SAME construction that decides
/// Rust's identity instead of a native re-implementation of it.
///
/// Canonical branch order is not decoration: it is the order every frame's
/// per-branch evidence is indexed by. A native sort that merely deduplicates
/// would still let `branches[0]` name a different branch than evidence entry
/// 0, and any drift in `Demand`'s ordering would silently desynchronize two
/// hand-written sorts from the Rust one. Delegating leaves exactly one
/// implementation of "same query".
///
/// Input branches are themselves live queries, so a nested aggregate result
/// limit is reachable and refused here rather than being quietly discarded.
/// Every refusal is the typed [`FfiError`] Rust produces for the same input.
#[uniffi::export]
pub fn live_query_union(
    branches: Vec<FfiLiveQuery>,
    aggregate_result_limit: Option<u32>,
) -> Result<FfiLiveQuery, crate::convert::FfiError> {
    let branches = branches
        .into_iter()
        .map(crate::convert::live_query_from_ffi)
        .collect::<Result<Vec<_>, _>>()?;
    let aggregate_result_limit = aggregate_result_limit.map(|limit| limit as usize);
    nmp::LiveQuery::union(branches, aggregate_result_limit)
        .map(crate::convert::live_query_to_ffi)
        .map_err(crate::convert::FfiError::from)
}

/// One delivered observation frame (`nmp::Frame` mirror) -- the ONE
/// vocabulary both observation modes share. Delivery is DERIVED from
/// boundedness, never a knob, and never carried twice on the wire:
///
/// - Unbounded (`window` is `None`): `deltas` is the exact transition rebased
///   onto the previously delivered frame. Intermediate reducer emits may be
///   conflated for a slow observer; the full row set is never redelivered
///   (full-set redelivery is the O(rows squared) P0 #485 exists to kill).
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
    /// This observation's acquisition evidence, ONE entry per canonical query
    /// branch in branch order (#1108). A single-branch live query carries
    /// exactly one entry. Branch identity is never erased and nothing here is
    /// a global completeness verdict.
    pub evidence: Vec<FfiAcquisitionEvidence>,
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

/// Failures that may resolve after a sign-only operation was accepted. Thrown
/// from [`crate::facade::NmpSignEventHandle::signed`] (#680).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Error)]
pub enum FfiSignEventFailure {
    SignerUnavailable {
        reason: String,
    },
    SignerRejected {
        reason: String,
    },
    InvalidSignerOutput {
        reason: String,
    },
    Cancelled,
    /// `NmpSignEventHandle::signed()` is one-shot: this is returned when
    /// `signed()` is awaited a second time (sequentially or concurrently) —
    /// the single result was already delivered to the first await (#680).
    AlreadyConsumed,
}

impl std::fmt::Display for FfiSignEventFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SignerUnavailable { reason } => write!(f, "signer unavailable: {reason}"),
            Self::SignerRejected { reason } => write!(f, "signer rejected request: {reason}"),
            Self::InvalidSignerOutput { reason } => {
                write!(f, "signer returned invalid output: {reason}")
            }
            Self::Cancelled => f.write_str("sign-only operation was cancelled"),
            Self::AlreadyConsumed => {
                f.write_str("sign-only result was already delivered to a prior signed() await")
            }
        }
    }
}

impl std::error::Error for FfiSignEventFailure {}

/// One tolerantly parsed Simple-groups item (#863, `nmp_nip51::
/// SimpleGroupEntry` mirror) -- group id, host relay, optional name.
/// `host_relay` is a canonically SPELLED observed string; it is not a
/// routing permission and no NIP-29 constructor accepts it implicitly.
#[derive(Debug, Clone, PartialEq, Eq, Record)]
pub struct FfiSimpleGroupEntry {
    pub group_id: String,
    pub host_relay: String,
    pub name: Option<String>,
}

/// NIP-51's tolerantly parsed Simple groups list (#863,
/// `nmp_nip51::SimpleGroupsList` mirror), with every evidence field
/// preserved across the FFI boundary. OBSERVATIONAL DATA ONLY: it may be
/// produced from a caller-constructed [`FfiRow`] of any kind, and it asserts
/// no signature, canonical-store, provenance, routing, or mutation
/// authority. There is deliberately no observation-qualified wrapper,
/// projection error, or frame proof around it.
#[derive(Debug, Clone, PartialEq, Eq, Record)]
pub struct FfiSimpleGroupsList {
    pub items: Vec<FfiSimpleGroupEntry>,
    pub relays_in_use: Vec<String>,
    pub malformed_item_count: u64,
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

/// `nmp::WriteRouting` mirror. BOTH words project, deliberately: `Auto`
/// ("figure out how to route whatever I'm publishing") and `Explicit`
/// ("use these exact relays and that is that").
///
/// An earlier premise held that letting an app route a write to a chosen
/// relay was a dangerous primitive, and this enum having exactly ONE variant
/// was the enforcement of that ban. The premise was reversed
/// outright (`docs/internals/routing/removed-routes.md` §2-3): publishing to
/// chosen relays is a first-class GENERAL capability with many legitimate
/// consumers — an app offering "publish this event to relay: [user input]",
/// a wiki crate publishing to the user's preferred wiki relays, a DM crate
/// publishing to two parties' DM relays, a group crate routing to its host,
/// and a user right-clicking someone else's note to archive it. Guarding it
/// is not protection, it is a defect, so `Explicit` is app-constructible
/// here on every platform.
///
/// What survives from the old shape is the tripwire, not its premise: this
/// exhaustive match still means a new `WriteRouting` variant landing in
/// `nmp-grammar` without a corresponding `FfiWriteRouting` decision is a
/// compile error, not a silent gap.
///
/// `relays` are relay-URL strings parsed at this boundary
/// (`convert::parse_relay_url`); a malformed one is a typed synchronous
/// [`crate::convert::FfiError::InvalidRelayUrl`] before any engine call. An
/// EMPTY list is refused at the engine's acceptance door — a routing rule
/// enforced once, identically, for every surface — with no receipt, no
/// journal row, and no fallback to `Auto`.
#[derive(Debug, Clone, PartialEq, Eq, Enum)]
pub enum FfiWriteRouting {
    Auto,
    Explicit { relays: Vec<String> },
}

/// `nmp::EventBuilder` mirror: the kind is demanded, everything else is
/// optional, and there is no author field because a builder has no author
/// until the engine resolves the write's identity at acceptance.
///
/// A UniFFI **Record with defaulted fields**, not an Object: Swift and
/// Kotlin's native idiom for "a record with defaults" IS a labeled-argument
/// initializer, so a Swift caller writes
/// `FfiEventBuilder(kind: 1, content: "hello")` and a Kotlin caller
/// `FfiEventBuilder(kind = 1u, content = "hello")`. A fluent object builder
/// would cross as an `Arc` handle whose combinators cannot consume `self`,
/// buying interior mutability and a round-trip per field for a type that is
/// four fields of data with no identity and no lifetime.
///
/// `created_at` absent means "stamp it at acceptance"; present means
/// exactly this timestamp, kept verbatim -- including one that loses a
/// replaceable race. Nothing here is validated: unrecognised tags cross
/// unchanged and a kind no module knows is published rather than refused.
#[derive(Debug, Clone, PartialEq, Eq, Record)]
pub struct FfiEventBuilder {
    pub kind: u16,
    #[uniffi(default = [])]
    pub tags: Vec<Vec<String>>,
    #[uniffi(default = "")]
    pub content: String,
    #[uniffi(default = None)]
    pub created_at: Option<u64>,
}

/// The event payload of a write intent (`nmp::WritePayload` mirror). VISION
/// P: signing and publishing are ORTHOGONAL stages -- `Event` describes an
/// event the engine stamps, freezes and signs internally ("the key lives in
/// the engine", ledger #12); `Signed` (#32, the M5 unlock) is a caller that
/// already holds a validly-signed event -- an external signer provider, or
/// a verbatim republish of somebody else's note to an archive relay -- and
/// hands its fields across as-is. `Signed`'s
/// fields are field-for-field [`FfiRow`] (the read-side mirror of a signed
/// `nostr::Event`) plus `sig`, deliberately: the write side stays symmetric
/// with the read side rather than introducing a JSON-blob shape.
///
/// There is no `ReplaceableEdit` mirror and never was: a CAS-guarded
/// replacement crosses this boundary only as a fused semantic method
/// (`NmpEngine::follow`/`unfollow`), which owns the evidence policy, the
/// precondition and the routing together. The native surface learns
/// `follow(target)`, not the pieces it would otherwise have to reassemble.
///
/// `Signed`'s fields are PARSED at this FFI boundary (typed hex/signature-
/// shape errors, see `convert::signed_event_from_ffi`) but NOT verified
/// here (#52 Unit B) -- `nostr::Event::verify` runs at
/// `nmp-engine::core::EngineCore::on_publish`'s acceptance boundary (Unit
/// A0/#56) instead, so the guarantee holds for every entry point, not only
/// this one. A tampered `Signed` event still parses fine here and is
/// rejected downstream, refusing the `publish` call as
/// `FfiError::PublishRefused` -- an instruction that cannot resolve is a
/// refusal, not a parked hope, so nothing is taken into custody. The engine
/// itself never re-signs, mutates a tag, or recomputes an id for this
/// variant.
#[derive(Debug, Clone, PartialEq, Eq, Enum)]
pub enum FfiWritePayload {
    Event {
        builder: FfiEventBuilder,
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

/// The identity one write publishes under (`nmp::Identity` mirror): two
/// variants, mirrored exactly, with no third "unset" state on any platform.
///
/// [`Active`](FfiIdentity::Active) is a positive instruction ("whoever is
/// the active account at acceptance"), not the absence of one -- which is
/// why this is a closed enum rather than the nullable pubkey string it
/// replaces. On an `Event` payload the identity SELECTS the author (a
/// builder states none, so there is nothing for it to contradict); on a
/// `Signed` payload it may only RESTATE the author already frozen in the
/// bytes, and naming anybody else is a consent/author contradiction
/// rejected by `publish` ITSELF as
/// [`FfiError::PublishRefused`](crate::convert::FfiError::PublishRefused) --
/// nothing is taken into custody, so no receipt and no queue entry exist.
#[derive(Debug, Clone, PartialEq, Eq, Enum)]
pub enum FfiIdentity {
    /// Whoever is the active account at acceptance time.
    Active,
    /// This exact key, active or not -- including while fully logged out.
    ///
    /// `pubkey` is 64-char HEX and nothing else: the module-wide
    /// `convert::parse_pubkey` rule every other pubkey input here follows.
    /// A bech32 `npub` is REFUSED, however well-formed -- bech32 is
    /// outward-facing decoration an app decodes at its own boundary (with
    /// `decode_nostr_entity`) at the moment a human pasted it, not an
    /// encoding the write plane accepts
    /// (`docs/internals/conventions/bech32-boundary.md`). A malformed
    /// string is a typed synchronous
    /// [`crate::convert::FfiError::InvalidPublicKey`] before any engine
    /// call.
    ///
    /// A key with no registered signer parks as
    /// [`FfiSigningState::AwaitingSigner`] (retained, not terminated)
    /// until that capability attaches. Acceptance PINS the resolved key
    /// either way, so a later `set_active_account` cannot retarget the
    /// write.
    Explicit { pubkey: String },
}

/// A caller's publish request (`nmp::WriteIntent` mirror).
#[derive(Debug, Clone, PartialEq, Eq, Record)]
pub struct FfiWriteIntent {
    pub payload: FfiWritePayload,
    pub routing: FfiWriteRouting,
    /// `nmp::WriteIntent::identity` mirror -- see [`FfiIdentity`].
    ///
    /// Unlike its Rust twin this field carries NO `#[uniffi(default = ...)]`
    /// and must be stated: UniFFI 0.29 record defaults accept only literals
    /// (`None`/`Some(lit)`/`[]`/int/float/bool/string), so an enum-valued
    /// default is not expressible at this boundary at all. The ergonomic
    /// native tiers (`NMP`'s `WriteIntent`, `com.nmp.sdk.WriteIntent`)
    /// default it to `.active` in their own language, which is where app
    /// code actually writes it.
    pub identity: FfiIdentity,
    /// `nmp_grammar::WriteIntent::correlation` mirror (#591): a caller-
    /// generated crash-safe correlation/idempotency token. `None` -- the
    /// default -- opts this write out of correlation entirely. `Some`
    /// crosses the boundary as a plain string and is validated by
    /// `nmp_grammar::CorrelationToken`'s `TryFrom<&str>` on the way in
    /// (non-empty, length-capped): a malformed token is a typed synchronous
    /// [`crate::convert::FfiError::InvalidCorrelationToken`] before any
    /// engine call. A token that already resolves to a previously-accepted
    /// receipt reattaches that existing obligation instead of enqueuing a
    /// second write -- see that type's doc for the full contract.
    #[uniffi(default = None)]
    pub correlation: Option<String>,
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
    pub nip77_handoff: String,
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

/// Where a durable write obligation is stuck (`nmp::StalledWriteStage`
/// mirror, #756/#968). Three stages, kept apart because an app acts on them
/// differently and because one rolled-up "stuck" tells nobody anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub enum FfiStalledWriteStage {
    /// No destination could be computed.
    Unroutable,
    /// No signer answers for the author this write was FROZEN to -- never
    /// the mutable active account.
    Unsignable,
    /// Destinations exist and none of them is working.
    Undeliverable,
}

/// One durable write obligation that cannot currently progress
/// (`nmp::StalledWrite` mirror). Read-only evidence: nothing here cancels,
/// retries, prunes or acknowledges a write.
#[derive(Debug, Clone, PartialEq, Eq, Record)]
pub struct FfiStalledWrite {
    /// A stable, restart-reproducible BLAKE3 descriptor of this obligation.
    /// Deliberately NOT a receipt id and deliberately not parseable back
    /// into one: it exists to tell two rows apart and to recognise the same
    /// row across snapshots, never to reattach or enumerate receipts.
    pub id: String,
    pub stage: FfiStalledWriteStage,
    /// What this write is waiting for. For `Unroutable` it is the receipt's
    /// OWN park reason, verbatim, so an operator holding both never has to
    /// decide whether two differently-worded sentences are the same fact.
    /// Never empty.
    pub detail: String,
    /// When the obligation was ACCEPTED, as a Unix timestamp in seconds,
    /// replayed verbatim across restarts. The age is `now - stalled_since`;
    /// NMP reports the instant rather than a duration because a duration
    /// baked into a snapshot goes stale exactly while nothing is happening.
    ///
    /// Known imprecision: this is when the OBLIGATION was accepted, not when
    /// the stall began. The two coincide for `Unroutable` and `Unsignable`;
    /// for `Undeliverable` it is EARLIER, so an app subtracting will
    /// over-report how long delivery has been failing. The park instant has
    /// no durable home yet, and an in-memory one would reset on every
    /// restart.
    pub stalled_since: u64,
}

/// The exact census behind `FfiDiagnosticsSnapshot.stalled_writes`
/// (`nmp::StalledWriteTotals` mirror). Totals count every stalled
/// obligation, including the ones no detail row was emitted for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Record)]
pub struct FfiStalledWriteTotals {
    pub unroutable: u64,
    pub unsignable: u64,
    pub undeliverable: u64,
    /// Stalled obligations with no detail row in this snapshot.
    pub omitted_details: u64,
    /// The detail-window bound this snapshot was built under.
    pub detail_limit: u64,
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
    /// Network-derived relay candidates rejected by the engine's SSRF
    /// admission policy (issue #121) before they could become router
    /// candidates or neutral route facts. This is a monotonic rejection-
    /// occurrence tally, not a distinct-host or per-direction count. A
    /// provider callback rejection counts once before directional projection;
    /// rejected selector evidence counts once when that exact
    /// `(selection, evidence)` first becomes current.
    pub discovered_private_relays_rejected: u64,
    /// Session dials the transport pool refused because the configured
    /// `max_relays` ceiling was already reached (issue #121, worker-exhaustion
    /// defense). Always `0` when no cap is configured.
    pub sessions_rejected_over_cap: u64,
    /// Latest transport acceptance/verifier failure, if any. This is
    /// observational diagnostics and never changes routing or trust policy.
    pub transport_degraded: Option<String>,
    /// Every durable write obligation that cannot progress, bounded to
    /// `stalled_write_totals.detail_limit` rows in a deterministic display
    /// order (stage, then acceptance instant, then descriptor).
    ///
    /// A receipt answers "what happened to THIS write", which needs someone
    /// still holding it; this answers "is anything quietly stuck" for an app
    /// holding nothing. Reading it changes nothing.
    pub stalled_writes: Vec<FfiStalledWrite>,
    /// Exact counts behind that window.
    pub stalled_write_totals: FfiStalledWriteTotals,
}

/// Exact authority whose typed answer terminally denied a write AUTH lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub enum FfiAuthDenialSource {
    Policy,
    Signer,
    Relay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub enum FfiRetryCause {
    Interrupted,
    AckTimeout,
    ConnectionLost,
    RelayRateLimited,
    RelayError,
}

/// `nmp::WriteFact` mirror: one fact about a write, delivered on its receipt
/// stream.
///
/// The old flat shape mixed facts about the whole write with facts about one
/// relay, which is why "is this terminal?" had no answer and every consumer
/// hand-wrote its own taxonomy (#1237). Here the two live on different arms
/// and [`Outcome`](FfiWriteFact::Outcome) is the only thing that ends
/// anything — so a stream can never end in silence, and an app can always
/// tell a finished write from a dropped subscription.
///
/// Acceptance is deliberately ABSENT: `publish` returning successfully IS
/// acceptance, so an app never has to ask the stream whether its write was
/// taken. Settlement is INSPECTED, never AWAITED.
#[derive(Debug, Clone, PartialEq, Eq, Enum)]
pub enum FfiWriteFact {
    Signing {
        state: FfiSigningState,
    },
    Relay {
        relay: String,
        state: FfiRelayState,
    },
    /// The relays this write is INTENDED for, and whether resolution can
    /// still change its mind. `complete` flips on settled RESOLUTION, never
    /// on delivery, so `complete: true` with nothing published yet is an
    /// ordinary state. This is the settlement denominator.
    ///
    /// `complete: false` with an empty set is a write still learning where it
    /// goes; it parks indefinitely and NOTHING expires it. `complete: true`
    /// with an empty set is [`FfiWriteOutcome::NoDestination`].
    Destinations {
        relays: Vec<String>,
        complete: bool,
    },
    Outcome {
        outcome: FfiWriteOutcome,
    },
}

/// `nmp::SigningState` mirror: the signing state of the WHOLE write — one
/// signature, one author, one answer.
#[derive(Debug, Clone, PartialEq, Eq, Enum)]
pub enum FfiSigningState {
    /// No registered signer answers for `pubkey` (64-char hex) — the exact
    /// identity FROZEN at acceptance, never whoever is active now. Re-armed
    /// only by attaching a signer for THIS key.
    ///
    /// **No clock ever ends this.** A device whose signer is simply not
    /// plugged in yet is not a device whose write failed; removing the queue
    /// entry is the only other exit.
    ///
    /// This is the state a person has to be told about, and
    /// [`FfiSigningState::InFlight`] is the one it must never be confused
    /// with.
    AwaitingSigner {
        pubkey: String,
    },
    /// A signer for `pubkey` (64-char hex) HAS the request and has not
    /// answered yet — the ordinary state of every healthy write between
    /// acceptance and signature promotion.
    ///
    /// Transient and normal: it ends when the signer answers (`Signed` or
    /// `Refused`), or falls back to `AwaitingSigner` if that signer becomes
    /// unavailable. Nothing here is a reason to trouble a user (#1261).
    InFlight {
        pubkey: String,
    },
    Signed {
        event_id: String,
    },
    /// The signer answered and said no. Terminal for the whole write.
    Refused {
        reason: String,
    },
}

/// `nmp::RelayState` mirror: what is true at ONE relay.
///
/// `Published`, `Rejected`, `AuthFailed` and `GaveUp` are terminal for that
/// relay; `Waiting` and `Sent` are not.
#[derive(Debug, Clone, PartialEq, Eq, Enum)]
pub enum FfiRelayState {
    Waiting {
        waiting: FfiRelayWaiting,
    },
    /// Transport proved socket write + flush. Not an ack, and not terminal.
    Sent {
        attempt: u64,
        written_at: u64,
    },
    Published,
    /// The relay authenticated the identity and refused THIS EVENT. The
    /// repair is to the event.
    Rejected {
        reason: String,
    },
    /// The write could not be authenticated HERE. Deliberately NOT folded
    /// into `Rejected`: `source` keeps an app's own decision not to
    /// authenticate from being reported to a user as a relay refusing them.
    AuthFailed {
        pubkey: String,
        source: FfiAuthDenialSource,
        reason: String,
    },
    /// The attempt ceiling was reached at this relay. Terminal HERE and
    /// nowhere else: three relays published and one given up on is a success
    /// with a footnote, not a failed write.
    GaveUp,
}

/// `nmp::RelayWaiting` mirror: why a relay lane is not attempting right now.
/// Every arm is a fact about the lane; none of them is a deadline.
#[derive(Debug, Clone, PartialEq, Eq, Enum)]
pub enum FfiRelayWaiting {
    /// Offline time consumes no attempt ordinal, so being offline can never
    /// spend the give-up ceiling.
    NotConnected,
    NeedsAuth,
    /// The last attempt failed in a way that permits another one, and
    /// `cause`/`detail` say WHY — "we will try again" and "we will try again
    /// because the relay rate-limited us" are different messages and only the
    /// second one can be acted on.
    BackingOff {
        attempt: u64,
        eligible_at: u64,
        cause: FfiRetryCause,
        detail: Option<String>,
    },
    /// The lane is owned and nonterminal, but a durable fact about it could
    /// not be committed — the local disk is refusing writes. No wire EVENT
    /// was emitted.
    ///
    /// Also LATCHED onto the queue entry
    /// ([`FfiPublishQueueEntry::persistence_fault`]) and never cleared by a
    /// later ack: an operator must not lose the only signal that the disk is
    /// failing because a relay succeeded afterwards.
    PersistenceStalled {
        detail: String,
    },
}

/// `nmp::WriteOutcome` mirror: the whole-write terminal. Exactly one of these
/// ends every receipt stream.
#[derive(Debug, Clone, PartialEq, Eq, Enum)]
pub enum FfiWriteOutcome {
    /// The destination set is CLOSED and every relay in it is terminal. What
    /// happened at each is the per-relay facts; this says only that no more
    /// are coming.
    Settled,
    /// Routing finished — knowledge is exhausted — and named zero relays.
    /// Terminal: there is nowhere to publish. Distinct from a route still
    /// resolving, which parks forever.
    NoDestination,
    /// The write ended without going anywhere.
    NotSent { reason: FfiNotSentReason },
    /// The store answered the acceptance instruction with a semantic no. The
    /// write is in custody as a permanently-failed entry: one row, payload
    /// intact, readable and removable through the queue door.
    Refused { reason: FfiRefuseReason },
}

/// `nmp::NotSentReason` mirror.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub enum FfiNotSentReason {
    Cancelled,
    /// A newer accepted write won the same replaceable coordinate before this
    /// one started any wire attempt. Not a failure — for an app renewing
    /// presence it is the steady state.
    Superseded,
    /// The app removed the queue entry while nothing was moving the write
    /// (#1269). Distinct from `Cancelled` in what survives: a cancelled
    /// receipt stays reattachable, a removed one no longer exists.
    Removed,
}

/// `nmp_store::RefuseReason` mirror: why the acceptance door said no.
#[derive(Debug, Clone, PartialEq, Eq, Enum)]
pub enum FfiRefuseReason {
    AlreadyExpired,
    Tombstoned,
    ReplaceableBaseOnRegularEvent,
    /// A whole-value replacement lost its compare-and-swap.
    ///
    /// BOTH ids are kept, and that is what makes the failure recoverable
    /// without the user: an app fetches `actual`, reapplies the change and
    /// resubmits silently. Reduced to a string it could only tell them to
    /// redo it.
    ReplaceableBaseChanged {
        expected: Option<String>,
        actual: Option<String>,
    },
}

/// `nmp::PublishQueueEntry` mirror: one write in the queue, as the app reads
/// it back (#1039).
///
/// Enumerating the queue answers "what have I got outstanding, and what went
/// wrong with it" without having held a receipt stream open since acceptance.
/// It is INSPECTION: nothing here blocks and nothing waits for settlement.
#[derive(Debug, Clone, PartialEq, Eq, Record)]
pub struct FfiPublishQueueEntry {
    pub receipt_id: u64,
    /// The frozen event id, 64-char hex — the write's identity from
    /// acceptance onward, unchanged by signing.
    pub event_id: String,
    /// The identity frozen at acceptance, 64-char hex. Never re-resolved.
    pub pubkey: String,
    pub accepted_at: u64,
    pub signing: FfiSigningState,
    pub relays: Vec<String>,
    pub route_complete: bool,
    pub relay_states: Vec<FfiQueueRelayState>,
    /// `None` while the write is still in progress.
    pub outcome: Option<FfiWriteOutcome>,
    /// LATCHED. Set the first time local persistence refused a durable fact
    /// for this write, and never cleared by a later success.
    pub persistence_fault: Option<String>,
}

/// One `(relay, state)` pair on a [`FfiPublishQueueEntry`]. A record rather
/// than a map because UniFFI dictionaries key only on primitives and the
/// relay URL is the caller-meaningful half.
#[derive(Debug, Clone, PartialEq, Eq, Record)]
pub struct FfiQueueRelayState {
    pub relay: String,
    pub state: FfiRelayState,
}

/// Typed refusal from the queue-entry removal door (#1039).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Error)]
pub enum FfiRemoveQueueEntryError {
    UnknownReceipt {
        receipt_id: u64,
    },
    /// Something is MOVING this write: a signer HAS its request and the
    /// answer is already on its way (`FfiSigningState::InFlight`), or it is
    /// signed and its relay lanes are live. Cancel it first; removal is for
    /// entries nothing is going to move — which a write parked on a signer
    /// nobody has is, so that one is removable (#1269).
    StillActive {
        receipt_id: u64,
    },
    PersistenceFailed {
        receipt_id: u64,
        reason: String,
    },
    EngineClosed,
}

impl std::fmt::Display for FfiRemoveQueueEntryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownReceipt { receipt_id } => write!(f, "unknown receipt {receipt_id}"),
            Self::StillActive { receipt_id } => write!(
                f,
                "receipt {receipt_id} still owns open delivery work; cancel it first"
            ),
            Self::PersistenceFailed { receipt_id, reason } => write!(
                f,
                "could not remove queue entry for receipt {receipt_id}: {reason}"
            ),
            Self::EngineClosed => write!(f, "engine already shut down"),
        }
    }
}

impl std::error::Error for FfiRemoveQueueEntryError {}

/// Typed refusal from explicit pre-signature write cancellation. The current
/// receipt fact survives intact when cancellation is no longer legal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub enum FfiCancelWriteOutcome {
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Error)]
pub enum FfiCancelWriteError {
    UnknownReceipt {
        receipt_id: u64,
    },
    AlreadySigned {
        receipt_id: u64,
        event_id: String,
    },
    AlreadyCompensated {
        receipt_id: u64,
    },
    AlreadySuperseded {
        receipt_id: u64,
    },
    /// The write was refused at acceptance and is already a permanently
    /// failed queue entry. There is nothing to cancel; remove it instead.
    AlreadyRefused {
        receipt_id: u64,
    },
    PersistenceFailed {
        receipt_id: u64,
        reason: String,
    },
    EngineClosed,
}

impl std::fmt::Display for FfiCancelWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownReceipt { receipt_id } => write!(f, "unknown receipt {receipt_id}"),
            Self::AlreadySigned {
                receipt_id,
                event_id,
            } => write!(f, "receipt {receipt_id} is already signed as {event_id}"),
            Self::AlreadyCompensated { receipt_id } => {
                write!(f, "receipt {receipt_id} is already compensated")
            }
            Self::AlreadySuperseded { receipt_id } => {
                write!(f, "receipt {receipt_id} was superseded by a newer write")
            }
            Self::AlreadyRefused { receipt_id } => {
                write!(f, "receipt {receipt_id} was refused at acceptance")
            }
            Self::PersistenceFailed { receipt_id, reason } => write!(
                f,
                "could not persist cancellation for receipt {receipt_id}: {reason}"
            ),
            Self::EngineClosed => write!(f, "engine already shut down"),
        }
    }
}

impl std::error::Error for FfiCancelWriteError {}

/// Result of looking up a stable retained receipt id. The `Attached` variant
/// carries the pull-based [`crate::facade::NmpReceiptStream`] that traverses
/// durable `WriteFact` facts in finite pages and streams onward (#680).
#[derive(uniffi::Enum)]
pub enum FfiReceiptReattachment {
    Attached {
        stream: Arc<crate::facade::NmpReceiptStream>,
    },
    NotFound,
    RetainedButUnreadable,
}

/// #591: `NmpEngine::reattachByCorrelation`'s result. Reuses
/// [`FfiReceiptReattachment`]'s exact three-way outcome vocabulary
/// unchanged -- this is not a new outcome enum, only a pairing with the
/// resolved receipt id a correlation-token caller cannot otherwise learn
/// (the by-id door needs no such pairing: the caller already supplied the
/// id). `receipt_id` is `Some` iff `outcome == Attached`, `None` otherwise.
#[derive(Record)]
pub struct FfiCorrelationReattachment {
    pub outcome: FfiReceiptReattachment,
    pub receipt_id: Option<u64>,
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

#[cfg(test)]
mod live_query_union_tests {
    use super::*;
    use crate::convert::FfiError;

    fn branch(relay: &str) -> FfiLiveQuery {
        FfiLiveQuery {
            branches: vec![FfiDemand {
                selection: FfiFilter {
                    kinds: Some(vec![1]),
                    authors: None,
                    ids: None,
                    tags: HashMap::new(),
                    since: None,
                    until: None,
                    limit: None,
                },
                source: FfiSourceAuthority::Pinned {
                    relays: vec![format!("wss://{relay}.example.com")],
                },
                access: FfiAccessContext::Public,
                cache: FfiCacheMode::Agnostic,
                freshness: FfiFreshness::Live,
            }],
            aggregate_result_limit: None,
        }
    }

    /// The door a hand-written SDK factory calls: permuting the declaration
    /// must produce byte-identical canonical output, since that output IS the
    /// native value's identity and its evidence indexing.
    #[test]
    fn permuted_declarations_produce_one_canonical_value() {
        let one_way = live_query_union(vec![branch("a"), branch("b")], None).unwrap();
        let other_way = live_query_union(vec![branch("b"), branch("a")], None).unwrap();
        assert_eq!(one_way, other_way);
        assert_eq!(one_way.branches.len(), 2);
    }

    #[test]
    fn a_repeated_branch_owns_exactly_one_canonical_entry() {
        let query = live_query_union(vec![branch("a"), branch("a")], None).unwrap();
        assert_eq!(query.branches.len(), 1);
    }

    #[test]
    fn the_aggregate_bound_survives_canonicalization() {
        let query = live_query_union(vec![branch("a"), branch("b")], Some(7)).unwrap();
        assert_eq!(query.aggregate_result_limit, Some(7));
    }

    #[test]
    fn every_refusal_is_the_typed_error_rust_produces() {
        assert!(matches!(
            live_query_union(Vec::new(), None),
            Err(FfiError::EmptyQueryUnion)
        ));
        assert!(matches!(
            live_query_union(vec![branch("a")], Some(0)),
            Err(FfiError::AggregateResultLimitZero)
        ));
        let bounded = live_query_union(vec![branch("a")], Some(3)).unwrap();
        assert!(matches!(
            live_query_union(vec![bounded], None),
            Err(FfiError::NestedAggregateResultLimit)
        ));
        let maximum = max_query_branches() as usize;
        let over_cap: Vec<_> = (0..=maximum).map(|k| branch(&k.to_string())).collect();
        assert!(matches!(
            live_query_union(over_cap, None),
            Err(FfiError::TooManyQueryBranches {
                requested,
                maximum: reported,
            }) if requested as usize == maximum + 1 && reported as usize == maximum
        ));
    }
}
