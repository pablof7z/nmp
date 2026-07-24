//! Coverage watermarks — implements the Fable ruling
//! (`docs/consults/2026-07-11-fable-coverage-attribution.md`) EXACTLY at the
//! store layer:
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
//!   every coverage row whose retained shape matches it and whose interval
//!   contains its `created_at`, in the same store transaction as the delete.
//!
//! **Attribution** (send-time snapshots, the intersection rule over
//! outstanding in-flight REQs, `limit` poisoning) is engine-owned per the
//! ruling (§2/§3) — `EngineCore` decides *whether* and *with what interval*
//! to call `record_coverage` at all. This module only has to make the
//! store-side half true: given a `(key, relay, interval)` it is told to
//! record, merge it soundly; given nothing, remember nothing.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;

use nmp_grammar::{fold_byte, ConcreteFilter, ContextualAtom, DescriptorHash, IndexedTagName};
use nostr::filter::MatchEventOptions;
use nostr::{Event, Timestamp};
use serde::{Deserialize, Serialize};

/// The `CoverageKey` schema version (#106, Fable's refinement of atlas's C
/// recommendation): folded into every key's HASH (below) and PREFIXED onto
/// its durable row key (`RedbStore::coverage_row_key`) — two independent
/// signals, so a legacy row is detectable both by string prefix (cheap,
/// what `gc`'s legacy-purge pass actually greps for) and would fail to
/// collide even if a caller somehow bypassed the prefix. v1 was the
/// pre-#106 scheme: bare `ConcreteFilter`, no context. v2 widens the
/// identity to a full [`ContextualAtom`] (`source`/`access` folded in) so
/// two Demands differing only in intended authority never share a coverage
/// row (bug-class ledger #18's store-side twin of the atom-refcount fix).
pub const COVERAGE_KEY_VERSION: u8 = 2;

/// The coverage identity of a narrow demand atom: its [`ContextualAtom`]
/// (selection + source + access, #106) with `since`/`until`/`limit` ERASED
/// from the selection, canonically hashed and version-tagged (ruling §1,
/// refined by Fable's C). Two atoms that differ only in their time window
/// or result cap hash identically — a floored refetch (`since = T+1`) must
/// find the SAME row, never a fresh one. Two atoms that differ in
/// `SourceAuthority`/`AccessContext` must NEVER share a row, even with an
/// otherwise-identical selection.
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
/// [`coverage_key`] (identity) and [`ClaimSet`] (GC matching) — both must
/// erase identically or the two notions of "shape" would silently diverge.
pub(crate) fn window_erase(filter: &ConcreteFilter) -> ConcreteFilter {
    ConcreteFilter {
        since: None,
        until: None,
        limit: None,
        ..filter.clone()
    }
}

/// The coverage key for `atom`'s window-erased shape UNDER its declared
/// `source`/`access` (ruling §1, #106-widened): version-tagged via
/// [`COVERAGE_KEY_VERSION`].
pub fn coverage_key(atom: &ContextualAtom) -> CoverageKey {
    let windowed = ContextualAtom {
        filter: window_erase(&atom.filter),
        source: atom.source.clone(),
        access: atom.access,
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
/// This is the ONLY merge algorithm in the crate — both `MemoryStore` and
/// `RedbStore` call it, so the oracle and the persistent backend can never
/// diverge on merge semantics.
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
/// (caller has already established `evicted_at` falls inside `interval` and
/// that the row's shape matches the evicted event — ruling §5). Keeps the
/// UPPER side (`[evicted_at + 1, through]`): LRU evicts OLD events, claims
/// protect recent ones, so the recent side is what live queries actually
/// rely on. Returns `None` when the shrink empties the interval — the
/// caller must then DELETE the row, in the same transaction as the event
/// delete (never claim coverage of data no longer held).
pub(crate) fn shrink_after_eviction(
    interval: CoverageInterval,
    evicted_at: Timestamp,
) -> Option<CoverageInterval> {
    let new_from = evicted_at + 1;
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

/// A one-shot index over a single `gc` call's victim set (issue #507),
/// shared verbatim by `MemoryStore::gc` and `RedbStore::gc` — the same
/// "one algorithm, both backends call it" pattern [`merge_interval`]
/// already establishes, so the two can never diverge on coverage-shrink
/// arithmetic.
///
/// **Why the maximum alone determines a row's outcome** (the fact this
/// type exists to exploit): [`shrink_after_eviction`] only ever RAISES
/// `interval.from`, to `evicted_at + 1`. Fix one coverage row and let `V`
/// be the set of victims that both match its shape and fall inside its
/// CURRENT `[from, through]`. Apply `shrink_after_eviction` for every
/// member of `V`, in any order:
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
///   alone: untouched if `V` is empty, else `from' = m + 1`, and the row
///   is deleted iff `from' > through` (same rule `shrink_after_eviction`
///   already encodes for a single victim).
///
/// This lets `gc` replace an O(victims × rows) nested loop (the eviction
/// pass's original shape, mirrored in both backends before issue #507)
/// with a single O(rows) pass: each row calls [`Self::max_matching_within`]
/// once, which walks its own pre-sorted, shape-pruned candidate slice
/// with an early exit on the first (descending-order) match, rather than
/// re-scanning every victim per row.
pub(crate) struct GcVictimIndex<'a> {
    /// Every victim, sorted ascending by `created_at`. Consulted only
    /// when a row's shape carries no concrete `kinds` set (nothing to
    /// prune by).
    global: Vec<&'a Event>,
    /// The same victims, ALSO bucketed by kind (each bucket sorted
    /// ascending by `created_at`) — consulted instead of `global`
    /// whenever the row's shape names a concrete kind set: a shape
    /// requiring kind `K` can never match a victim of a different kind,
    /// so its search only ever has to walk the buckets for its own
    /// kinds, never the other victims at all.
    by_kind: HashMap<u16, Vec<&'a Event>>,
}

impl<'a> GcVictimIndex<'a> {
    /// Build the index once per `gc` call, from the victims that call
    /// already collected (owned `Event`s — both backends gather the full
    /// victim set up front, before touching any coverage row).
    pub(crate) fn new(victims: &'a [Event]) -> Self {
        let mut global: Vec<&'a Event> = victims.iter().collect();
        global.sort_by_key(|event| event.created_at);

        let mut by_kind: HashMap<u16, Vec<&'a Event>> = HashMap::new();
        for event in victims {
            by_kind.entry(event.kind.as_u16()).or_default().push(event);
        }
        for bucket in by_kind.values_mut() {
            bucket.sort_by_key(|event| event.created_at);
        }

        Self { global, by_kind }
    }

    /// `m` from this type's own doc comment: the greatest `created_at`
    /// among victims that both match `shape` and fall inside `interval`
    /// — or `None` if no victim qualifies at all (the row is then left
    /// untouched by the caller). Walks candidates in DESCENDING
    /// `created_at` order so the very FIRST shape match encountered is
    /// already the maximum — a true early exit, never a full pass over
    /// the qualifying range.
    pub(crate) fn max_matching_within(
        &self,
        shape: &ConcreteFilter,
        interval: CoverageInterval,
    ) -> Option<Timestamp> {
        // `shape.kinds` mirrors `nostr::Filter::kind_match`'s own
        // semantics (via `ConcreteFilter::to_nostr`/`shape_matches`):
        // `Some(non_empty)` restricts to those kinds, but `None` OR
        // `Some(empty)` both mean "no kind constraint" (matches any kind)
        // -- `kind_match`'s `kinds.is_empty() || kinds.contains(..)` is
        // vacuously `true` for an empty required set. Pruning by kind
        // bucket is only sound for the genuinely-restrictive case; an
        // empty-but-`Some` set must fall back to the unpruned global scan
        // exactly like `None`, or this would wrongly report "no victim
        // matches" for a shape that in fact matches every kind.
        match shape.kinds.as_ref().filter(|kinds| !kinds.is_empty()) {
            Some(kinds) => {
                // Coarse shape-fingerprint pruning: only the buckets for
                // the shape's own kinds can possibly match it. Each
                // bucket's own local max is independent of the others,
                // so the overall answer is just the max across every
                // qualifying kind's bucket.
                let mut best: Option<Timestamp> = None;
                for kind in kinds {
                    let Some(bucket) = self.by_kind.get(kind) else {
                        continue;
                    };
                    if let Some(found) = Self::scan_descending(bucket, shape, interval) {
                        best = Some(best.map_or(found, |current| current.max(found)));
                    }
                }
                best
            }
            None => Self::scan_descending(&self.global, shape, interval),
        }
    }

    /// `candidates` must already be sorted ascending by `created_at`.
    /// Binary-searches (`partition_point`) to the sub-slice inside
    /// `[interval.from, interval.through]`, then walks it back-to-front —
    /// the first `shape_matches` hit in that reverse walk is the maximum
    /// matching `created_at`, so this returns on the first hit rather
    /// than visiting the whole qualifying range.
    fn scan_descending(
        candidates: &[&Event],
        shape: &ConcreteFilter,
        interval: CoverageInterval,
    ) -> Option<Timestamp> {
        let start = candidates.partition_point(|event| event.created_at < interval.from);
        let end = candidates.partition_point(|event| event.created_at <= interval.through);
        candidates[start..end]
            .iter()
            .rev()
            .find(|event| shape_matches(shape, event))
            .map(|event| event.created_at)
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
/// all — see [`crate::EventStore::gc`]), are retained.
#[derive(Debug, Clone, Default)]
pub struct ClaimSet {
    claims: Vec<ConcreteFilter>,
}

impl ClaimSet {
    /// Build a `ClaimSet` from the caller's demand skeletons. Defensively
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

/// The result of a [`crate::EventStore::gc`] call.
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
    /// Legacy-schema coverage rows purged outright (#106, Fable's C
    /// refinement): a row whose durable key predates the current
    /// `CoverageKey` schema version is permanently orphaned (nothing will
    /// ever compute a matching key for it again), so `gc` deletes it
    /// unconditionally rather than let it linger. Disjoint from
    /// `coverage_rows_deleted` (which is specifically shrink-emptied
    /// current-schema rows) so a test/operator can distinguish "ordinary
    /// GC deleted this" from "this was a leftover from before a schema
    /// migration".
    pub legacy_coverage_rows_purged: usize,
}

/// A window-erased `ConcreteFilter` shape, JSON-encodable for durable
/// storage. `ConcreteFilter` itself has no `serde` derive (out of scope to
/// add — that would touch `nmp-grammar`), so `RedbStore` retains coverage
/// rows via this mirror struct instead: every field is a plain,
/// JSON-representable type, and the two `From` conversions below are the
/// only place the mapping is written down.
///
/// This is *why* `CoverageRow` carries more than the ruling's minimal sketch
/// (`key`/`relay`/`from`/`through`): `gc()` must be able to test "does this
/// evicted event match this row's shape" for EVERY row, including rows for
/// shapes no longer part of any live demand — and a hash is one-way, so the
/// store must retain the shape it was given at `record_coverage` time to
/// make that test possible at all. The `CoverageKey`/`get_coverage`/
/// `record_coverage` contract (lookup identity, merge/refuse-floor
/// semantics) is unchanged; this is purely an internal retention detail
/// needed to implement ruling §5.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ShapeRecord {
    kinds: Option<BTreeSet<u16>>,
    authors: Option<BTreeSet<String>>,
    ids: Option<BTreeSet<String>>,
    tags: BTreeMap<String, BTreeSet<String>>,
}

impl From<&ConcreteFilter> for ShapeRecord {
    fn from(f: &ConcreteFilter) -> Self {
        ShapeRecord {
            kinds: f.kinds.clone(),
            authors: f.authors.clone(),
            ids: f.ids.clone(),
            tags: f
                .tags
                .iter()
                .map(|(k, v)| (k.as_char().to_string(), v.clone()))
                .collect(),
        }
    }
}

impl From<&ShapeRecord> for ConcreteFilter {
    fn from(r: &ShapeRecord) -> Self {
        ConcreteFilter {
            kinds: r.kinds.clone(),
            authors: r.authors.clone(),
            ids: r.ids.clone(),
            tags: r
                .tags
                .iter()
                .map(|(k, v)| {
                    let c = k
                        .chars()
                        .next()
                        .expect("ShapeRecord tag keys are always single characters (see From<&ConcreteFilter>)");
                    (
                        IndexedTagName::new(c).expect(
                            "ShapeRecord tag keys were validated IndexedTagNames when persisted",
                        ),
                        v.clone(),
                    )
                })
                .collect(),
            since: None,
            until: None,
            limit: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap as StdBTreeMap, BTreeSet as StdBTreeSet};

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

    /// Wrap a filter into a fixed-context (`AuthorOutboxes`/`Public`) demand
    /// atom -- these tests exercise the SELECTION axis of `coverage_key`;
    /// the context-anti-alias property has its own dedicated falsifier
    /// below.
    fn atom(filter: ConcreteFilter) -> ContextualAtom {
        ContextualAtom {
            filter,
            source: nmp_grammar::SourceAuthority::AuthorOutboxes,
            access: nmp_grammar::AccessContext::Public,
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
    /// selection under different `SourceAuthority` must never share a
    /// coverage row.
    #[test]
    fn coverage_key_differs_for_different_source_authority() {
        let filter = cf(&[1], &["aa"], None, None);
        let outbox = ContextualAtom {
            filter: filter.clone(),
            source: nmp_grammar::SourceAuthority::AuthorOutboxes,
            access: nmp_grammar::AccessContext::Public,
            routing_evidence: BTreeSet::new(),
        };
        let public = ContextualAtom {
            filter,
            source: nmp_grammar::SourceAuthority::Public,
            access: nmp_grammar::AccessContext::Public,
            routing_evidence: BTreeSet::new(),
        };
        assert_ne!(coverage_key(&outbox), coverage_key(&public));
    }

    /// #49's access-context anti-alias falsifier: a proven public interval
    /// cannot satisfy the identical selection acquired through an
    /// authenticated NIP-42 session (or vice versa).
    #[test]
    fn coverage_key_differs_for_different_access_context() {
        let filter = cf(&[1], &["aa"], None, None);
        let public = ContextualAtom {
            filter: filter.clone(),
            source: nmp_grammar::SourceAuthority::AuthorOutboxes,
            access: nmp_grammar::AccessContext::Public,
            routing_evidence: BTreeSet::new(),
        };
        let authenticated = ContextualAtom {
            filter,
            source: nmp_grammar::SourceAuthority::AuthorOutboxes,
            access: nmp_grammar::AccessContext::Nip42(nostr::Keys::generate().public_key()),
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
    fn shape_record_round_trips_through_conversion() {
        let mut tags = StdBTreeMap::new();
        tags.insert(
            IndexedTagName::new('d').unwrap(),
            StdBTreeSet::from(["g1".to_string()]),
        );
        let original = ConcreteFilter {
            kinds: Some(StdBTreeSet::from([30_003u16])),
            authors: Some(StdBTreeSet::from(["aa".to_string()])),
            ids: None,
            tags,
            since: None,
            until: None,
            limit: None,
        };

        let record = ShapeRecord::from(&original);
        let restored: ConcreteFilter = (&record).into();
        assert_eq!(original, restored);
    }

    // -----------------------------------------------------------------
    // `GcVictimIndex` (issue #507): the shared gc coverage-shrink batching
    // helper both backends call, so they can never diverge on this
    // arithmetic — see the type's own doc comment for the max-only-
    // matters proof these tests exercise.
    // -----------------------------------------------------------------

    fn victim(keys: &nostr::Keys, kind: u16, created_at: u64) -> Event {
        nostr::EventBuilder::new(nostr::Kind::from(kind), "")
            .custom_created_at(Timestamp::from(created_at))
            .sign_with_keys(keys)
            .unwrap()
    }

    fn kind_shape(kind: u16) -> ConcreteFilter {
        ConcreteFilter {
            kinds: Some(StdBTreeSet::from([kind])),
            authors: None,
            ids: None,
            tags: StdBTreeMap::new(),
            since: None,
            until: None,
            limit: None,
        }
    }

    fn any_kind_shape() -> ConcreteFilter {
        ConcreteFilter {
            kinds: None,
            authors: None,
            ids: None,
            tags: StdBTreeMap::new(),
            since: None,
            until: None,
            limit: None,
        }
    }

    #[test]
    fn max_matching_within_returns_none_when_no_victim_matches() {
        let keys = nostr::Keys::generate();
        let victims = vec![victim(&keys, 1, 10)];
        let index = GcVictimIndex::new(&victims);
        let interval = CoverageInterval::new(Timestamp::from(100u64), Timestamp::from(200u64));
        // The one victim's `created_at` (10) is outside the interval.
        assert!(index
            .max_matching_within(&kind_shape(1), interval)
            .is_none());
    }

    #[test]
    fn max_matching_within_picks_the_greatest_matching_created_at() {
        let keys = nostr::Keys::generate();
        let victims = vec![
            victim(&keys, 1, 50),
            victim(&keys, 1, 100),
            victim(&keys, 1, 75),
        ];
        let index = GcVictimIndex::new(&victims);
        let interval = CoverageInterval::new(Timestamp::from(0u64), Timestamp::from(200u64));
        assert_eq!(
            index.max_matching_within(&kind_shape(1), interval),
            Some(Timestamp::from(100u64)),
            "the maximum matching created_at must win, regardless of insertion order"
        );
    }

    #[test]
    fn max_matching_within_prunes_non_matching_kinds_even_inside_the_interval() {
        let keys = nostr::Keys::generate();
        // A kind-3 victim sitting squarely inside the interval must never
        // be considered for a kind-1-shaped row: shape pruning walks only
        // the kind-1 bucket, which is empty here.
        let victims = vec![victim(&keys, 3, 50)];
        let index = GcVictimIndex::new(&victims);
        let interval = CoverageInterval::new(Timestamp::from(0u64), Timestamp::from(100u64));
        assert!(index
            .max_matching_within(&kind_shape(1), interval)
            .is_none());
    }

    #[test]
    fn max_matching_within_falls_back_to_the_global_list_for_kindless_shapes() {
        let keys = nostr::Keys::generate();
        let victims = vec![victim(&keys, 1, 10), victim(&keys, 9, 20)];
        let index = GcVictimIndex::new(&victims);
        let interval = CoverageInterval::new(Timestamp::from(0u64), Timestamp::from(100u64));
        assert_eq!(
            index.max_matching_within(&any_kind_shape(), interval),
            Some(Timestamp::from(20u64))
        );
    }

    /// `nostr::Filter::kind_match` (via `ConcreteFilter::to_nostr`/
    /// `shape_matches`) treats an empty-but-`Some` kinds set as "no kind
    /// constraint at all" — vacuously matching every kind — the identical
    /// convention `Filter::authors_match`/`ids_match` use for their own
    /// empty-`Some` sets. Kind-bucket pruning must therefore treat
    /// `Some(empty)` exactly like `None` (fall back to the unpruned
    /// global list), never as "matches nothing": pruning on the buckets
    /// named by an empty set would otherwise silently disagree with
    /// `shape_matches` and report no victim at all for a shape that in
    /// fact matches every one of them.
    #[test]
    fn max_matching_within_treats_empty_kinds_set_as_no_constraint_not_as_impossible() {
        let keys = nostr::Keys::generate();
        let victims = vec![victim(&keys, 1, 10), victim(&keys, 9, 20)];
        let index = GcVictimIndex::new(&victims);
        let interval = CoverageInterval::new(Timestamp::from(0u64), Timestamp::from(100u64));
        let empty_kinds_shape = ConcreteFilter {
            kinds: Some(StdBTreeSet::new()),
            authors: None,
            ids: None,
            tags: StdBTreeMap::new(),
            since: None,
            until: None,
            limit: None,
        };
        assert_eq!(
            index.max_matching_within(&empty_kinds_shape, interval),
            Some(Timestamp::from(20u64)),
            "an empty-but-Some kinds set must behave like no kind constraint at all"
        );
    }
}
