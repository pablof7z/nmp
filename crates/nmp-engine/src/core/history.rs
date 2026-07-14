use std::sync::Arc;

use nmp_resolver::LiveQuery;
use nmp_store::EventCursor;

use super::{AcquisitionEvidence, Row, RowDelta};

pub(crate) const HISTORY_CONTINUATION_VERSION: u16 = 1;

/// A validated declaration for one coordinated, bounded history session.
///
/// This remains a specialization of NMP's read noun: the selection/source/
/// access/cache identity is the ordinary [`LiveQuery`], while `page_size`
/// and `max_rows` bound only the session's active projection. A history
/// selection cannot also carry NIP-01 `limit`; that would create two
/// competing owners for row membership.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryQuery {
    query: LiveQuery,
    page_size: usize,
    max_rows: usize,
}

impl HistoryQuery {
    pub fn new(
        query: LiveQuery,
        page_size: usize,
        max_rows: usize,
    ) -> Result<Self, HistoryQueryError> {
        if page_size == 0 {
            return Err(HistoryQueryError::ZeroPageSize);
        }
        if max_rows == 0 {
            return Err(HistoryQueryError::ZeroMaxRows);
        }
        if page_size > max_rows {
            return Err(HistoryQueryError::PageExceedsMaxRows {
                page_size,
                max_rows,
            });
        }
        if query.0.selection.limit.is_some() {
            return Err(HistoryQueryError::SelectionHasLimit);
        }
        Ok(Self {
            query,
            page_size,
            max_rows,
        })
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

    pub(crate) fn older_demand(&self, created_at: u64) -> Option<LiveQuery> {
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
        selection.limit = Some(self.page_size);
        Some(LiveQuery(demand))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryQueryError {
    ZeroPageSize,
    ZeroMaxRows,
    PageExceedsMaxRows { page_size: usize, max_rows: usize },
    SelectionHasLimit,
}

impl std::fmt::Display for HistoryQueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroPageSize => f.write_str("history page_size must be non-zero"),
            Self::ZeroMaxRows => f.write_str("history max_rows must be non-zero"),
            Self::PageExceedsMaxRows {
                page_size,
                max_rows,
            } => write!(
                f,
                "history page_size {page_size} exceeds max_rows {max_rows}"
            ),
            Self::SelectionHasLimit => {
                f.write_str("history selection must not also declare a limit")
            }
        }
    }
}

impl std::error::Error for HistoryQueryError {}

/// Opaque process-local capability for advancing one exact session state.
/// Every field is private: callers can only return a value NMP issued.
#[derive(Debug, Clone)]
pub struct HistoryContinuation {
    pub(crate) version: u16,
    pub(crate) engine_identity: Arc<()>,
    pub(crate) session_identity: Arc<()>,
    pub(crate) descriptor: LiveQuery,
    pub(crate) generation: u64,
    pub(crate) boundary: EventCursor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryLoadFact {
    Idle,
    Requesting,
    Returned { added: usize },
    AtBound { max_rows: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryLoadError {
    WrongVersion,
    WrongEngine,
    WrongSession,
    WrongDescriptor,
    StaleGeneration,
    LoadInProgress,
    AtBound { max_rows: usize },
    NoBoundary,
    StoreUnavailable,
    TransportUnavailable { reason: String },
}

impl std::fmt::Display for HistoryLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongVersion => f.write_str("history continuation version is unsupported"),
            Self::WrongEngine => f.write_str("history continuation belongs to another engine"),
            Self::WrongSession => f.write_str("history continuation belongs to another session"),
            Self::WrongDescriptor => {
                f.write_str("history continuation belongs to another demand descriptor")
            }
            Self::StaleGeneration => f.write_str("history continuation is stale"),
            Self::LoadInProgress => f.write_str("history session already has a staged load"),
            Self::AtBound { max_rows } => {
                write!(f, "history session is at its max_rows bound {max_rows}")
            }
            Self::NoBoundary => f.write_str("history session has no row boundary to advance"),
            Self::StoreUnavailable => {
                f.write_str("history advance could not read or resolve the canonical store")
            }
            Self::TransportUnavailable { reason } => {
                write!(f, "history advance transport unavailable: {reason}")
            }
        }
    }
}

impl std::error::Error for HistoryLoadError {}

/// One self-contained bounded history frame.
///
/// `rows` is the authoritative canonical current set, ordered newest-first.
/// `deltas` describes the transition from the reducer's immediately prior
/// state; the runtime history receiver re-derives it from its own last
/// delivered `rows` after latest-wins coalescing, so skipped frames never
/// create a lossy incremental contract.
#[derive(Debug, Clone)]
pub struct HistoryBatch {
    pub rows: Vec<Row>,
    pub deltas: Vec<RowDelta>,
    pub continuation: Option<HistoryContinuation>,
    pub evidence: AcquisitionEvidence,
    pub load: HistoryLoadFact,
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
    fn declaration_rejects_every_competing_or_unbounded_shape() {
        assert_eq!(
            HistoryQuery::new(query(), 0, 10),
            Err(HistoryQueryError::ZeroPageSize)
        );
        assert_eq!(
            HistoryQuery::new(query(), 1, 0),
            Err(HistoryQueryError::ZeroMaxRows)
        );
        assert_eq!(
            HistoryQuery::new(query(), 11, 10),
            Err(HistoryQueryError::PageExceedsMaxRows {
                page_size: 11,
                max_rows: 10,
            })
        );
        let mut limited = query();
        limited.0.selection.limit = Some(5);
        assert_eq!(
            HistoryQuery::new(limited, 5, 10),
            Err(HistoryQueryError::SelectionHasLimit)
        );
    }

    #[test]
    fn acquisition_windows_keep_tie_proof_distinct_from_limited_older_work() {
        let history = HistoryQuery::new(query(), 5, 20).unwrap();
        assert_eq!(history.initial_demand().0.selection.limit, Some(5));

        let tie = history.tie_second_demand(100).unwrap();
        assert_eq!(tie.0.selection.since, Some(100));
        assert_eq!(tie.0.selection.until, Some(100));
        assert_eq!(tie.0.selection.limit, None);

        let older = history.older_demand(100).unwrap();
        assert_eq!(older.0.selection.until, Some(99));
        assert_eq!(older.0.selection.limit, Some(5));
        assert!(history.older_demand(0).is_none());
    }
}
