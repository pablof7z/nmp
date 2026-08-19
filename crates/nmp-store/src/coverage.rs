//! Coverage watermarks — the store half of
//! `docs/design/query-demand-and-evidence.md` and issue #816's
//! facts-before-claims contract:
//!
//! - Coverage is keyed by the NARROW atom's **window-erased** shape hash
//!   ([`CoverageKey`]) — never by a wide wire filter. `since`/`until`/`limit`
//!   are cleared before hashing (§1); the time window lives in the row's
//!   [`CoverageInterval`], never in the key.
//! - A row asserts a proven `[covered_from, covered_through]` interval, not a
//!   downward-closed `[0, T]` (ruling §1's deliberate, justified deviation
//!   from the harvested doctrine: GC-split honesty + M4 pagination).
//! - `record_coverage` only merges/advances (no row → insert; overlapping or
//!   adjacent → union; disjoint → keep the interval with the greater
//!   `through`, recency wins) (§3). It has NO public lowering path.
//! - `get_coverage` returns `None` when no row exists — "no row = not
//!   covered", the harvested refuse-the-floor rule, unchanged.
//! - Lowering happens ONLY inside `gc()` (§5): evicting an event shrinks
//!   every coverage row whose interval contains its `created_at`, in the
//!   same store transaction as the delete. The row is NOT asked whether the
//!   evicted event was one it would have matched — see [`GcVictimIndex`] for
//!   why that question is both unanswerable from a row and unnecessary.
//!
//! **Attribution** (send-time snapshots, the intersection rule over
//! outstanding in-flight REQs, `limit` poisoning) is engine-owned per the
//! ruling (§2/§3) — `EngineCore` decides *whether* and *with what interval*
//! to call `record_coverage` at all. This module only has to make the
//! store-side half true: given a `(key, relay, interval)` it is told to
//! record, merge it soundly; given nothing, remember nothing.

use std::collections::BTreeSet;

use nmp_grammar::{ConcreteFilter, ContextualAtom};
use nostr::filter::MatchEventOptions;
use nostr::{Event, Timestamp};

/// Erase `since`/`until`/`limit` from `filter`, leaving `kinds`/`authors`/
/// `ids`/`tags` untouched. This is the ONE erasure rule shared by
/// [`coverage_key`] (identity) and [`GcRetentionSet`] (GC matching) — both must
/// erase identically or the two notions of "shape" would silently diverge.
pub(crate) fn window_erase(filter: &ConcreteFilter) -> ConcreteFilter {
    ConcreteFilter {
        since: None,
        until: None,
        limit: None,
        ..filter.clone()
    }
}

/// The coverage identity of a narrow demand atom: its [`ContextualAtom`]
/// (selection + routing + identity, #106) with `since`/`until`/`limit`
/// ERASED from the selection and routing evidence dropped.
///
/// It holds the ATOM, not a digest of one. A hash could answer exactly one
/// question -- "is this byte-identical to something I saw before?" -- which
/// is the least useful question available about a filter. Holding the atom
/// lets a caller ask the questions that matter: does this one CONTAIN that
/// one, what is the residual between them, which axis do they differ on.
/// Those are set operations on the selection, and a 32-byte BLAKE3 digest
/// destroys exactly the information they need.
///
/// Two atoms differing only in their time window or result cap share a key,
/// because the window is not part of coverage identity. Two atoms differing
/// in `ReadRouting`, or in the identity they authenticate as, never do.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CoverageKey(ContextualAtom);

impl CoverageKey {
    /// The window-erased atom this key IS.
    pub fn atom(&self) -> &ContextualAtom {
        &self.0
    }
}

/// The coverage key for `atom`'s window-erased shape under its routing and
/// authenticated identity.
pub fn coverage_key(atom: &ContextualAtom) -> CoverageKey {
    let windowed = ContextualAtom {
        filter: window_erase(&atom.filter),
        routing: atom.routing.clone(),
        authenticate_as: atom.authenticate_as,
        routing_evidence: BTreeSet::new(),
    };
    CoverageKey(windowed)
}

/// A proven, retained interval `[from, through]` (ruling §1's `CoverageRow`,
/// minus the identity fields that live in the store's key space). `from` is
/// `0` in the common unfloored case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoverageInterval {
    pub from: Timestamp,
    pub through: Timestamp,
}

impl CoverageInterval {
    pub fn new(from: Timestamp, through: Timestamp) -> Self {
        Self { from, through }
    }
}

/// Merge `incoming` into `existing` (ruling §3):
/// - no row → insert `incoming` outright;
/// - overlapping OR adjacent (`incoming.from <= existing.through + 1` AND
///   `incoming.through >= existing.from - 1`, both saturating) → union
///   (extend either end);
/// - disjoint → keep whichever interval has the greater `through` (recency
///   wins); the discarded interval costs bandwidth, never correctness.
///
/// This is the only merge algorithm in the crate, shared by every Redb
/// coverage mutation path.
pub(crate) fn merge_interval(
    existing: Option<CoverageInterval>,
    incoming: CoverageInterval,
) -> CoverageInterval {
    let Some(cur) = existing else {
        return incoming;
    };

    let touches = incoming.from <= cur.through + 1 && incoming.through >= cur.from - 1;
    if touches {
        CoverageInterval {
            from: cur.from.min(incoming.from),
            through: cur.through.max(incoming.through),
        }
    } else if incoming.through > cur.through {
        incoming
    } else {
        cur
    }
}

/// Shrink `interval` after evicting an event observed at `evicted_at`
/// (caller has already established `evicted_at` falls inside `interval` —
/// ruling §5). Keeps the
/// UPPER side (`[successor(evicted_at), through]`): LRU evicts OLD events,
/// claims protect recent ones, so the recent side is what live queries
/// actually rely on. Returns `None` when `evicted_at` has no representable
/// successor or the shrink otherwise empties the interval — the caller must
/// then DELETE the row, in the same transaction as the event delete (never
/// claim coverage of data no longer held).
pub(crate) fn shrink_after_eviction(
    interval: CoverageInterval,
    evicted_at: Timestamp,
) -> Option<CoverageInterval> {
    let new_from = evicted_at
        .as_secs()
        .checked_add(1)
        .map(Timestamp::from_secs)?;
    if new_from > interval.through {
        None
    } else {
        Some(CoverageInterval {
            from: new_from,
            through: interval.through,
        })
    }
}

/// True iff `event` falls inside `shape`'s (already window-erased)
/// `kinds`/`authors`/`ids`/`tags` — delegated entirely to
/// `nostr::Filter::match_event` (memory rule: use rust-nostr, not scratch
/// matching logic), never re-implemented by hand.
pub(crate) fn shape_matches(shape: &ConcreteFilter, event: &Event) -> bool {
    shape
        .to_nostr()
        .match_event(event, MatchEventOptions::new())
}

/// A one-shot index over a single `gc` call's victim set (issue #507), used by
/// `RedbStore::gc` to keep coverage-shrink arithmetic in one place. It is
/// nothing but the victims' `created_at` values, sorted: **a coverage row's
/// outcome depends only on timestamps** (#1849).
///
/// **Why a row is never asked what it would have matched.** Coverage is a
/// cache of "I already fetched this range from this relay", and its two
/// error directions are not symmetric: believing coverage is SMALLER than
/// reality costs a refetch, believing it LARGER is a correctness bug,
/// because coverage feeds confident-absence decisions. So shrinking every
/// row whose interval contains an evicted event — without asking whether
/// that event matched the row — is always sound. It over-invalidates, and
/// over-invalidation costs bandwidth.
///
/// The precision the alternative would buy is not worth its price: testing
/// "did this evicted event match this row" requires the store to durably
/// RETAIN the filter each row was recorded against (a hash is one-way), so
/// NMP kept every distinct query shape a user had ever issued — authors,
/// ids, tags and kinds — on disk, forever, with no expiry and no delete
/// door. No filter is stored in the database (#1849).
///
/// **Why the maximum alone determines a row's outcome** (the fact this
/// type exists to exploit): [`shrink_after_eviction`] only ever RAISES
/// `interval.from`, to the representable successor of `evicted_at`, or
/// deletes the interval when no successor exists. Fix one coverage row and
/// let `V` be the set of victims falling inside its CURRENT
/// `[from, through]`. Apply `shrink_after_eviction` for every member of
/// `V`, in any order:
///
/// - A victim `v` only has any effect if `v.created_at` is still inside
///   the interval at the moment it is applied. Once `from` has been
///   raised past `v.created_at` (by processing some OTHER victim first),
///   `v` falls outside the interval and applying it is a no-op.
/// - So the only victims that can ever actually move `from` are those
///   whose `created_at` is `>=` every `from`-raise applied before them —
///   which telescopes to: only the run culminating in the single LARGEST
///   `created_at` in `V` ever survives to set the final `from`. Every
///   smaller victim either fires first and is immediately superseded by
///   a later, larger one, or fires after the max and is already outside
///   the (already-raised) interval and is a no-op.
/// - Therefore the row's final state after processing every member of
///   `V`, IN ANY ORDER, is identical to processing just `m = max(V)`
///   alone: untouched if `V` is empty, else `from' = successor(m)`. The row
///   is deleted if that successor does not exist or is greater than
///   `through` (the same rule `shrink_after_eviction` already encodes for a
///   single victim).
///
/// This lets `gc` replace an O(victims × rows) nested loop with a single
/// O(rows log victims) pass: each row calls [`Self::max_within`] once,
/// which binary-searches the sorted timestamps and reads off the last one
/// in range.
pub(crate) struct GcVictimIndex {
    /// Every victim's `created_at`, sorted ascending.
    created_at: Vec<Timestamp>,
}

impl GcVictimIndex {
    /// Build the index once per `gc` call, from the victims that call
    /// already collected (owned `Event`s gathered before touching any
    /// coverage row).
    pub(crate) fn new(victims: &[Event]) -> Self {
        let mut created_at: Vec<Timestamp> = victims.iter().map(|event| event.created_at).collect();
        created_at.sort_unstable();
        Self { created_at }
    }

    /// `m` from this type's own doc comment: the greatest victim
    /// `created_at` inside `interval` — or `None` if no victim falls in
    /// range at all (the row is then left untouched by the caller).
    /// `partition_point` binary-searches to the end of the qualifying
    /// sub-slice; its last element IS the maximum, because the vector is
    /// sorted.
    pub(crate) fn max_within(&self, interval: CoverageInterval) -> Option<Timestamp> {
        let start = self
            .created_at
            .partition_point(|created_at| *created_at < interval.from);
        let end = self
            .created_at
            .partition_point(|created_at| *created_at <= interval.through);
        self.created_at[start..end].last().copied()
    }
}

/// The union of every live query's demand skeletons (VISION plan §3.1): what
/// a live handle still needs, as WINDOW-ERASED `ConcreteFilter` shapes
/// (ruling §5: "claim matching must be window-erased too" — a live query
/// with `since:X` still claims its shape's older events for
/// coverage-integrity purposes, even though it would not itself re-fetch
/// them).
///
/// `gc()` may evict only events matched by NO claim; a claimed event, and
/// every replaceable/addressable current winner (never a GC candidate at
/// all — see [`crate::RedbStore::gc`]), are retained.
#[derive(Debug, Clone, Default)]
pub struct GcRetentionSet {
    claims: Vec<ConcreteFilter>,
}

impl GcRetentionSet {
    /// Build a `GcRetentionSet` from the caller's demand skeletons. Defensively
    /// window-erases every claim itself (never trusts the caller to have
    /// already done so) — the invariant holds even if a caller forgets.
    pub fn new(claims: Vec<ConcreteFilter>) -> Self {
        Self {
            claims: claims.iter().map(window_erase).collect(),
        }
    }

    /// True iff `event` matches at least one live claim.
    pub(crate) fn is_claimed(&self, event: &Event) -> bool {
        self.claims.iter().any(|c| shape_matches(c, event))
    }
}

/// The result of a [`crate::RedbStore::gc`] call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcReport {
    /// Regular (non-replaceable, non-addressable) events evicted because no
    /// live claim matched them.
    pub events_evicted: usize,
    /// Coverage rows whose interval shrank because an evicted event fell
    /// inside their proven range (but did not empty the interval).
    pub coverage_rows_shrunk: usize,
    /// Coverage rows deleted because the shrink emptied their interval.
    pub coverage_rows_deleted: usize,
}

