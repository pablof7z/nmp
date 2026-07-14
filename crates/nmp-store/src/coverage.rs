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
}
