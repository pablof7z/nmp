use nmp_grammar::{Demand, LiveQuery};

use super::{AcquisitionEvidence, Row, RowDelta};

/// A validated declaration for one coordinated, bounded window session (#485).
///
/// This remains a specialization of NMP's read noun: the selection/source/
/// access/cache identity is the ordinary [`LiveQuery`], while `page_size`
/// (the window's initial row count) and `max_rows` (its declared ceiling)
/// bound only the session's active projection. A windowed selection cannot
/// also carry NIP-01 `limit`; that would create two competing owners for row
/// membership.
///
/// The public facade (`crates/nmp`) validates `initial <= max` and the
/// no-selection-limit rule BEFORE constructing this value (surfacing typed
/// `EngineError`s), so the constructor is infallible and only debug-asserts
/// those invariants. `NonZeroUsize` at the facade makes the zero cases
/// unrepresentable before they ever reach here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryQuery {
    query: LiveQuery,
    page_size: usize,
    max_rows: usize,
}

impl HistoryQuery {
    /// Construct an already-valid window declaration. `page_size` is the
    /// initial window size; `max_rows` is the declared growth ceiling. The
    /// facade guarantees `0 < page_size <= max_rows` and that `query` has no
    /// NIP-01 `limit`; those are debug-asserted, never re-reported as a
    /// public error (there is no dead public error enum for a state the
    /// facade already made unrepresentable).
    #[must_use]
    pub fn new(query: LiveQuery, page_size: usize, max_rows: usize) -> Self {
        debug_assert!(page_size > 0, "window initial size must be non-zero");
        debug_assert!(max_rows > 0, "window max_rows must be non-zero");
        debug_assert!(
            page_size <= max_rows,
            "window initial size must not exceed max_rows"
        );
        debug_assert!(
            query
                .branches()
                .iter()
                .all(|branch| branch.selection.limit.is_none()),
            "windowed selection must not also declare a NIP-01 limit"
        );
        debug_assert!(
            query.aggregate_result_limit().is_none(),
            "a window and an aggregate result limit are two owners of row membership"
        );
        Self {
            query,
            page_size,
            max_rows,
        }
    }

    #[must_use]
    pub fn live_query(&self) -> &LiveQuery {
        &self.query
    }

    #[must_use]
    pub const fn page_size(&self) -> usize {
        self.page_size
    }

    #[must_use]
    pub const fn max_rows(&self) -> usize {
        self.max_rows
    }

    /// The initial bounded acquisition for each canonical branch, in branch
    /// order. Each branch asks for the window's initial row count of its OWN
    /// selection: taking the newest `page_size` from every branch is exact,
    /// because a row outside one branch's newest `page_size` already has that
    /// many newer witnesses in that same branch. The global bound is applied
    /// once to the merged union, never per branch.
    pub(crate) fn initial_demands(&self) -> Vec<Demand> {
        self.query
            .branches()
            .iter()
            .map(|branch| {
                let mut demand = branch.clone();
                demand.selection.limit = Some(self.page_size);
                demand
            })
            .collect()
    }

    /// The exact tie-second acquisition for each branch that can contain
    /// `created_at`, paired with its canonical branch index. A branch whose
    /// own selection excludes that second contributes nothing.
    pub(crate) fn tie_second_demands(&self, created_at: u64) -> Vec<(usize, Demand)> {
        self.query
            .branches()
            .iter()
            .enumerate()
            .filter_map(|(index, branch)| {
                let mut demand = branch.clone();
                let selection = &mut demand.selection;
                if selection.since.is_some_and(|since| since > created_at)
                    || selection.until.is_some_and(|until| until < created_at)
                {
                    return None;
                }
                selection.since = Some(created_at);
                selection.until = Some(created_at);
                selection.limit = None;
                Some((index, demand))
            })
            .collect()
    }

    /// The bounded older-range acquisition for one advance, per branch.
    /// `limit` is the number of rows still needed to reach the current
    /// target (the actual advance chunk `new_target - already_held`), not a
    /// fixed page size: `request_rows(at_least)` can raise the target by an
    /// arbitrary amount, so each branch must ask for exactly the shortfall.
    pub(crate) fn older_demands(&self, created_at: u64, limit: usize) -> Vec<(usize, Demand)> {
        let Some(older_until) = created_at.checked_sub(1) else {
            return Vec::new();
        };
        self.query
            .branches()
            .iter()
            .enumerate()
            .filter_map(|(index, branch)| {
                let mut demand = branch.clone();
                let selection = &mut demand.selection;
                let until = selection
                    .until
                    .map_or(older_until, |existing| existing.min(older_until));
                if selection.since.is_some_and(|since| since > until) {
                    return None;
                }
                selection.until = Some(until);
                selection.limit = Some(limit);
                Some((index, demand))
            })
            .collect()
    }
}

/// Mechanical growth state of an expandable window, delivered as a fact in
/// every window frame (#485). This is the exact vocabulary the facade
/// re-exports and the FFI/Swift/Kotlin layers mirror.
///
/// Deliberately no `Complete`/`End`/`Synced` variant: `Returned { added: 0 }`
/// only means the planned advance added no canonical row (the per-source
/// [`AcquisitionEvidence`] carried alongside says whether that is a true
/// bound or merely an as-yet-unanswered relay). `AtBound { max }` is the only
/// terminal fact, and it means the declared ceiling was reached — it is a
/// FACT in a frame, never a thrown error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum WindowLoad {
    Idle,
    Requesting,
    Returned { added: usize },
    AtBound { max: usize },
}

/// The surviving advance failures for `request_rows` (#485). Every prior
/// continuation-token misuse variant (`WrongVersion`/`WrongEngine`/
/// `WrongSession`/`WrongDescriptor`/`StaleGeneration`) is gone: growth is
/// declarative (`at_least: usize`), so there is no opaque token to mismatch.
/// `LoadInProgress`/`AtBound`/`NoBoundary` are gone too: an in-flight advance
/// simply raises the target, and being at the bound is a frame fact. What
/// remains is canonical-store failure while staging an advance. The facade
/// maps it into its public `RequestRowsError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryAdvanceError {
    /// The canonical store could not read or resolve the advance; the staged
    /// load was rolled back with exact prior-projection restoration.
    StoreUnavailable,
}

impl std::fmt::Display for HistoryAdvanceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StoreUnavailable => {
                f.write_str("window advance could not read or resolve the canonical store")
            }
        }
    }
}

impl std::error::Error for HistoryAdvanceError {}

/// One self-contained bounded window frame.
///
/// `rows` is the authoritative canonical current set, ordered newest-first.
/// `deltas` describes the transition from the reducer's immediately prior
/// state; the runtime window receiver re-derives it from its own last
/// delivered `rows` after latest-wins coalescing, so skipped frames never
/// create a lossy incremental contract. (The public facade drops `deltas` on
/// the wire for bounded windows — delivery is a conflated snapshot, derived
/// from boundedness; rows never cross the FFI boundary twice.)
#[derive(Debug, Clone)]
pub struct HistoryBatch {
    pub rows: Vec<Row>,
    pub deltas: Vec<RowDelta>,
    /// Per-BRANCH acquisition evidence in canonical branch order (#1108).
    pub evidence: Vec<AcquisitionEvidence>,
    pub load: WindowLoad,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HistorySessionId(pub(crate) u64);

