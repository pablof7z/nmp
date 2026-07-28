use nmp_resolver::LiveQuery;

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
            query.0.selection.limit.is_none(),
            "windowed selection must not also declare a NIP-01 limit"
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

    pub(crate) fn initial_demand(&self) -> LiveQuery {
        let mut demand = self.query.0.clone();
        demand.selection.limit = Some(self.page_size);
        LiveQuery(demand)
    }

    pub(crate) fn tie_second_demand(&self, created_at: u64) -> Option<LiveQuery> {
        let mut demand = self.query.0.clone();
        let selection = &mut demand.selection;
        if selection.since.is_some_and(|since| since > created_at)
            || selection.until.is_some_and(|until| until < created_at)
        {
            return None;
        }
        selection.since = Some(created_at);
        selection.until = Some(created_at);
        selection.limit = None;
        Some(LiveQuery(demand))
    }

    /// The bounded older-range acquisition for one advance. `limit` is the
    /// number of rows still needed to reach the current target (the actual
    /// advance chunk `new_target - already_held`), not a fixed page size:
    /// `request_rows(at_least)` can raise the target by an arbitrary amount,
    /// so the wire request must ask for exactly the shortfall.
    pub(crate) fn older_demand(&self, created_at: u64, limit: usize) -> Option<LiveQuery> {
        let older_until = created_at.checked_sub(1)?;
        let mut demand = self.query.0.clone();
        let selection = &mut demand.selection;
        let until = selection
            .until
            .map_or(older_until, |existing| existing.min(older_until));
        if selection.since.is_some_and(|since| since > until) {
            return None;
        }
        selection.until = Some(until);
        selection.limit = Some(limit);
        Some(LiveQuery(demand))
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
    pub evidence: AcquisitionEvidence,
    pub load: WindowLoad,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HistorySessionId(pub(crate) u64);

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use nmp_grammar::{Binding, Filter};

    use super::*;

    fn query() -> LiveQuery {
        LiveQuery::from_filter(Filter {
            authors: Some(Binding::Literal(BTreeSet::from(["11".repeat(32)]))),
            ..Filter::default()
        })
    }

    #[test]
    fn acquisition_windows_keep_tie_proof_distinct_from_limited_older_work() {
        let history = HistoryQuery::new(query(), 5, 20);
        assert_eq!(history.initial_demand().0.selection.limit, Some(5));

        let tie = history.tie_second_demand(100).unwrap();
        assert_eq!(tie.0.selection.since, Some(100));
        assert_eq!(tie.0.selection.until, Some(100));
        assert_eq!(tie.0.selection.limit, None);

        // The older range asks for exactly the advance chunk it is handed,
        // not a fixed page size.
        let older = history.older_demand(100, 3).unwrap();
        assert_eq!(older.0.selection.until, Some(99));
        assert_eq!(older.0.selection.limit, Some(3));
        assert!(history.older_demand(0, 3).is_none());
    }
}
