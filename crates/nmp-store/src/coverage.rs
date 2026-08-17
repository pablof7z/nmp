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

use nmp_grammar::{fold_byte, ConcreteFilter, ContextualAtom, DescriptorHash};
use nostr::filter::MatchEventOptions;
use nostr::{Event, Timestamp};

/// The `CoverageKey` schema version (#106): folded into every key's HASH
/// (below) and PREFIXED onto its durable row key
/// (`RedbStore::coverage_row_key`). The current identity is the full
/// [`ContextualAtom`] (routing and the demand's identity folded in), so two
/// Demands that would authenticate as different keys never share a coverage
/// row (bug-class ledger #18's store-side twin of the atom-refcount fix).
///
/// It is a schema tag, not a compatibility discriminator: no reader decodes a
/// different version, and `gc` has no purge pass for one (#867).
///
/// Bumped 2 -> 3 for the durable row-key format change alone. The DIGEST is
/// byte-identical to version 2 for every value the fold can take, so this
/// buys no correctness on its own and no aliasing bug motivated it.
pub const COVERAGE_KEY_VERSION: u8 = 3;

/// The coverage identity of a narrow demand atom: its [`ContextualAtom`]
/// (selection + routing + identity, #106) with `since`/`until`/`limit` ERASED
/// from the selection, canonically hashed and version-tagged (ruling §1,
/// refined by Fable's C). Two atoms that differ only in their time window
/// or result cap hash identically — a floored refetch (`since = T+1`) must
/// find the SAME row, never a fresh one. Two atoms that differ in
/// `ReadRouting`, or in the identity they authenticate as, must NEVER share
/// a row, even with an otherwise-identical selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CoverageKey(DescriptorHash);

impl CoverageKey {
    /// The raw 32-byte BLAKE3 digest, for use as (part of) a durable
    /// storage key. Widened from a 64-bit FNV hash (see
    /// `nmp_grammar::DescriptorHash`'s doc): this is the durable redb
    /// coverage-watermark key, so a collision here would attach a proven
    /// interval to a filter never actually fetched.
    pub fn as_bytes(&self) -> &[u8; 32] {
        self.0.as_bytes()
    }
}

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

/// The coverage key for `atom`'s window-erased shape UNDER its `source` and
/// authenticated identity (ruling §1, #106-widened): version-tagged via
/// [`COVERAGE_KEY_VERSION`].
pub fn coverage_key(atom: &ContextualAtom) -> CoverageKey {
    let windowed = ContextualAtom {
        filter: window_erase(&atom.filter),
        routing: atom.routing.clone(),
        authenticate_as: atom.authenticate_as,
        routing_evidence: BTreeSet::new(),
    };
    CoverageKey(fold_byte(windowed.hash(), COVERAGE_KEY_VERSION))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap as StdBTreeMap;

    fn cf(
        kinds: &[u16],
        authors: &[&str],
        since: Option<u64>,
        limit: Option<usize>,
    ) -> ConcreteFilter {
        ConcreteFilter {
            kinds: Some(kinds.iter().copied().collect()),
            authors: Some(authors.iter().map(|s| s.to_string()).collect()),
            ids: None,
            tags: StdBTreeMap::new(),
            since,
            until: None,
            limit,
        }
    }

    /// Wrap a filter into a fixed-context (`Auto`, unauthenticated) demand
    /// atom -- these tests exercise the SELECTION axis of `coverage_key`;
    /// the context-anti-alias property has its own dedicated falsifier
    /// below.
    fn atom(filter: ConcreteFilter) -> ContextualAtom {
        ContextualAtom {
            filter,
            routing: nmp_grammar::ReadRouting::Auto,
            authenticate_as: None,
            routing_evidence: BTreeSet::new(),
        }
    }

    #[test]
    fn coverage_key_ignores_since_until_limit() {
        let a = cf(&[1], &["aa"], Some(100), Some(50));
        let b = cf(&[1], &["aa"], Some(999), None);
        assert_eq!(coverage_key(&atom(a)), coverage_key(&atom(b)));
    }

    #[test]
    fn coverage_key_differs_for_different_shapes() {
        let a = cf(&[1], &["aa"], None, None);
        let b = cf(&[1], &["bb"], None, None);
        assert_ne!(coverage_key(&atom(a)), coverage_key(&atom(b)));
    }

    /// `CoverageKey` is the DURABLE redb watermark key (ledger #7): a forged
    /// collision here attaches evidence to the wrong filter. Pin its width at 32 bytes
    /// (256-bit BLAKE3, via `DescriptorHash`) -- NOT the 8-byte FNV-64 value
    /// it replaced -- so a future change can't silently narrow it back down.
    #[test]
    fn coverage_key_is_a_256_bit_digest_not_64() {
        let a = cf(&[1], &["aa"], None, None);
        assert_eq!(coverage_key(&atom(a)).as_bytes().len(), 32);
    }

    /// Same filter hashed twice (simulating a re-derive across two separate
    /// calls, e.g. two different code paths computing the same atom's
    /// coverage key) is byte-for-byte stable -- required for `get_coverage`/
    /// `record_coverage` to ever find the SAME durable row twice.
    #[test]
    fn coverage_key_is_stable_across_repeated_calls() {
        let a = atom(cf(&[1], &["aa", "bb"], Some(10), Some(5)));
        assert_eq!(coverage_key(&a).as_bytes(), coverage_key(&a).as_bytes());
    }

    /// #106's store-side anti-alias (Fable's C refinement, ledger #18's
    /// twin of the resolver-side `ContextualAtom` fix): the IDENTICAL
    /// selection under different `ReadRouting` must never share a
    /// coverage row.
    #[test]
    fn coverage_key_differs_for_different_read_routing() {
        let filter = cf(&[1], &["aa"], None, None);
        let auto = ContextualAtom {
            filter: filter.clone(),
            routing: nmp_grammar::ReadRouting::Auto,
            authenticate_as: None,
            routing_evidence: BTreeSet::new(),
        };
        let explicit = ContextualAtom {
            filter,
            routing: nmp_grammar::ReadRouting::Explicit(vec![nostr::RelayUrl::parse(
                "wss://coverage-anti-alias.example",
            )
            .unwrap()]),
            authenticate_as: None,
            routing_evidence: BTreeSet::new(),
        };
        assert_ne!(
            coverage_key(&auto),
            coverage_key(&explicit),
            "what one relay set proved must never satisfy a demand routed anywhere NMP chooses"
        );
    }

    /// #49's access-context anti-alias falsifier: a proven public interval
    /// cannot satisfy the identical selection acquired through an
    /// authenticated NIP-42 session (or vice versa).
    #[test]
    fn coverage_key_differs_for_different_access_context() {
        let filter = cf(&[1], &["aa"], None, None);
        let public = ContextualAtom {
            filter: filter.clone(),
            routing: nmp_grammar::ReadRouting::Auto,
            authenticate_as: None,
            routing_evidence: BTreeSet::new(),
        };
        let authenticated = ContextualAtom {
            filter,
            routing: nmp_grammar::ReadRouting::Auto,
            authenticate_as: Some(nostr::Keys::generate().public_key()),
            routing_evidence: BTreeSet::new(),
        };
        assert_ne!(coverage_key(&public), coverage_key(&authenticated));
    }

    #[test]
    fn coverage_key_erases_routing_evidence() {
        let plain = atom(cf(&[1], &["aa"], None, None));
        let mut hinted = plain.clone();
        hinted
            .routing_evidence
            .insert(nmp_grammar::RoutingEvidence {
                relay: nostr::RelayUrl::parse("wss://hint.example").unwrap(),
                origin: nmp_grammar::RoutingEvidenceKind::Hint,
            });
        assert_eq!(coverage_key(&plain), coverage_key(&hinted));
    }

    #[test]
    fn merge_with_no_existing_row_inserts_outright() {
        let incoming = CoverageInterval::new(Timestamp::from(10u64), Timestamp::from(20u64));
        assert_eq!(merge_interval(None, incoming), incoming);
    }

    #[test]
    fn merge_extends_on_overlap() {
        let existing = CoverageInterval::new(Timestamp::from(0u64), Timestamp::from(100u64));
        let incoming = CoverageInterval::new(Timestamp::from(50u64), Timestamp::from(150u64));
        let merged = merge_interval(Some(existing), incoming);
        assert_eq!(merged.from, Timestamp::from(0u64));
        assert_eq!(merged.through, Timestamp::from(150u64));
    }

    #[test]
    fn merge_extends_on_exact_adjacency() {
        // Planner floors REQs at covered_through + 1: the common contiguous
        // extension path.
        let existing = CoverageInterval::new(Timestamp::from(0u64), Timestamp::from(100u64));
        let incoming = CoverageInterval::new(Timestamp::from(101u64), Timestamp::from(200u64));
        let merged = merge_interval(Some(existing), incoming);
        assert_eq!(merged.from, Timestamp::from(0u64));
        assert_eq!(merged.through, Timestamp::from(200u64));
    }

    #[test]
    fn merge_keeps_greater_through_on_disjoint_intervals() {
        let existing = CoverageInterval::new(Timestamp::from(0u64), Timestamp::from(100u64));
        // A gap: 102..200 does not touch 0..100 (102 > 100+1).
        let incoming = CoverageInterval::new(Timestamp::from(102u64), Timestamp::from(200u64));
        let merged = merge_interval(Some(existing), incoming);
        assert_eq!(
            merged, incoming,
            "recency wins: the greater `through` survives"
        );

        // And the reverse: an older, smaller-through disjoint interval never
        // overwrites a newer one.
        let existing2 = CoverageInterval::new(Timestamp::from(300u64), Timestamp::from(400u64));
        let incoming2 = CoverageInterval::new(Timestamp::from(0u64), Timestamp::from(50u64));
        let merged2 = merge_interval(Some(existing2), incoming2);
        assert_eq!(merged2, existing2);
    }

    #[test]
    fn shrink_after_eviction_keeps_upper_side() {
        let interval = CoverageInterval::new(Timestamp::from(0u64), Timestamp::from(100u64));
        let shrunk = shrink_after_eviction(interval, Timestamp::from(50u64)).unwrap();
        assert_eq!(shrunk.from, Timestamp::from(51u64));
        assert_eq!(shrunk.through, Timestamp::from(100u64));
    }

    #[test]
    fn shrink_after_eviction_returns_none_when_emptied() {
        let interval = CoverageInterval::new(Timestamp::from(100u64), Timestamp::from(100u64));
        assert!(shrink_after_eviction(interval, Timestamp::from(100u64)).is_none());
    }

    #[test]
    fn shrink_after_eviction_drops_the_maximum_timestamp_boundary() {
        let interval = CoverageInterval::new(Timestamp::max(), Timestamp::max());
        assert!(
            shrink_after_eviction(interval, Timestamp::max()).is_none(),
            "u64::MAX has no representable upper-side successor"
        );
    }

    // -----------------------------------------------------------------
    // `GcVictimIndex` (issue #507): the shared gc coverage-shrink batching
    // helper every Redb path calls, so they cannot diverge on this arithmetic
    // — see the type's own doc comment for the max-only-
    // matters proof these tests exercise.
    // -----------------------------------------------------------------

    fn victim(keys: &nostr::Keys, kind: u16, created_at: u64) -> Event {
        nostr::EventBuilder::new(nostr::Kind::from(kind), "")
            .custom_created_at(Timestamp::from(created_at))
            .sign_with_keys(keys)
            .unwrap()
    }

    #[test]
    fn max_within_returns_none_when_no_victim_falls_in_range() {
        let keys = nostr::Keys::generate();
        let victims = vec![victim(&keys, 1, 10)];
        let index = GcVictimIndex::new(&victims);
        let interval = CoverageInterval::new(Timestamp::from(100u64), Timestamp::from(200u64));
        // The one victim's `created_at` (10) is outside the interval.
        assert!(index.max_within(interval).is_none());
    }

    #[test]
    fn max_within_picks_the_greatest_created_at_in_range() {
        let keys = nostr::Keys::generate();
        let victims = vec![
            victim(&keys, 1, 50),
            victim(&keys, 1, 100),
            victim(&keys, 1, 75),
        ];
        let index = GcVictimIndex::new(&victims);
        let interval = CoverageInterval::new(Timestamp::from(0u64), Timestamp::from(200u64));
        assert_eq!(
            index.max_within(interval),
            Some(Timestamp::from(100u64)),
            "the maximum created_at in range must win, regardless of insertion order"
        );
    }

    #[test]
    fn max_within_excludes_victims_outside_the_interval_on_both_sides() {
        let keys = nostr::Keys::generate();
        let victims = vec![
            victim(&keys, 1, 10),
            victim(&keys, 1, 50),
            victim(&keys, 1, 200),
        ];
        let index = GcVictimIndex::new(&victims);
        let interval = CoverageInterval::new(Timestamp::from(40u64), Timestamp::from(100u64));
        assert_eq!(index.max_within(interval), Some(Timestamp::from(50u64)));
    }

    /// The coarser rule (#1849): a victim inside a row's interval shrinks
    /// that row whatever it is, because a row no longer carries — and the
    /// database no longer stores — the filter it was recorded against.
    /// Over-invalidation costs a refetch; the opposite error would be a
    /// correctness bug, since coverage feeds confident-absence decisions.
    #[test]
    fn max_within_counts_a_victim_of_any_kind_inside_the_interval() {
        let keys = nostr::Keys::generate();
        let victims = vec![victim(&keys, 3, 50), victim(&keys, 9, 60)];
        let index = GcVictimIndex::new(&victims);
        let interval = CoverageInterval::new(Timestamp::from(0u64), Timestamp::from(100u64));
        assert_eq!(
            index.max_within(interval),
            Some(Timestamp::from(60u64)),
            "no victim is excluded on the grounds of what it looks like"
        );
    }
}
