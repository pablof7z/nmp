//! [`LiveQuery`] — the app-facing read declaration (#1108).
//!
//! One live query is one or more complete, independent [`Demand`] branches
//! observed through ONE lifecycle. `Demand` itself stays atomic: it owns a
//! selection, a routing, an access context, a cache mode and a
//! freshness policy, and it never nests another `Demand` except at the exact
//! `Derived` boundary the grammar already defines. Composition of independent
//! branches is a property of the DECLARATION, not of `Demand`, which is why
//! it lives here and not on the descriptor.
//!
//! Some correct reads need several branches whose results form one semantic
//! query, and whose scoped values must not cross between them. A NIP-29
//! listing derived from evidence observed at relay A must constrain the outer
//! listing at A and never at relay B; flattening the two hosts into one
//! `ReadRouting::Explicit({A, B})` produces a confidently wrong
//! cross-product, and handing the app a `Vec<Demand>` makes the app own the
//! aggregate observation NMP promises to own. A `LiveQuery` with two branches
//! expresses exactly that and stays one observation.

use std::collections::BTreeSet;

use crate::descriptor::Demand;

/// The app-facing read declaration: a nonempty, canonical set of complete
/// [`Demand`] branches plus the optional aggregate row-membership bound that
/// applies to their union.
///
/// # Canonical value
///
/// Branch order is the `Demand` [`Ord`] order, and exact duplicates
/// collapse. Two declarations built from permuted, nested or repeated inputs
/// are therefore the SAME value with the same hash, and index into the same
/// per-branch evidence order in every delivered frame. Declaration order is
/// never an observable.
///
/// Two branches that differ only in `cache` or `freshness` remain two
/// branches: those are per-handle policies, deliberately outside acquisition
/// identity, so collapsing them would silently discard one branch's owned
/// policy. Safe graph/wire/coverage sharing between them still happens one
/// level down, where it is invisible to this declaration.
///
/// # Two owners of row membership never coexist
///
/// [`Self::aggregate_result_limit`] bounds the union AFTER branch rows are
/// merged by event id — never `N` rows per branch presented as one `N`-row
/// result. It is distinct from a branch's own NIP-01 `selection.limit`, which
/// bounds only that branch's selection and is never widened, stripped, or
/// reinterpreted as a global bound.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LiveQuery {
    branches: Vec<Demand>,
    aggregate_result_limit: Option<usize>,
}

/// The unconstructible [`LiveQuery`] declarations (#1108).
///
/// Every case here is refused at construction, before an observation, a
/// handle, a mailbox, a graph claim or a wire request can exist. A capacity
/// refusal names the exact counts rather than silently installing a subset of
/// the requested branches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveQueryError {
    /// A union was constructed from no branches at all. There is nothing to
    /// observe and no honest evidence to report about it.
    EmptyUnion,
    /// An aggregate result limit of zero was requested. A query that may
    /// never contain a row is not a bound, it is an unobservable declaration.
    AggregateResultLimitZero,
    /// A nested branch carried its own aggregate result limit. Branches
    /// flatten into ONE canonical set, so an inner aggregate bound has no
    /// surviving scope to bound — accepting it would silently discard it.
    NestedAggregateResultLimit,
    /// The canonical branch count exceeds the supported hard ceiling. The
    /// whole declaration is refused; no subset is installed.
    TooManyQueryBranches { requested: usize, maximum: usize },
}

impl std::fmt::Display for LiveQueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyUnion => f.write_str("a live query must declare at least one demand branch"),
            Self::AggregateResultLimitZero => {
                f.write_str("an aggregate result limit of zero can never contain a row")
            }
            Self::NestedAggregateResultLimit => f.write_str(
                "a nested live-query branch must not declare its own aggregate result limit",
            ),
            Self::TooManyQueryBranches { requested, maximum } => write!(
                f,
                "a live query supports at most {maximum} demand branches; {requested} were declared"
            ),
        }
    }
}

impl std::error::Error for LiveQueryError {}

impl LiveQuery {
    /// The hard ceiling on canonical branches in one observation.
    ///
    /// Every branch is a complete, independently planned demand with its own
    /// graph claim, atoms, wire participation and evidence entry. The ceiling
    /// is a refusal, never a truncation: an over-cap declaration produces
    /// [`LiveQueryError::TooManyQueryBranches`] and installs nothing.
    pub const MAX_BRANCHES: usize = 64;

    /// One complete demand observed on its own. Identical in lifecycle,
    /// frame shape and evidence shape to a union of one — a single query is
    /// not a second kind of observation.
    #[must_use]
    pub fn single(branch: Demand) -> Self {
        Self {
            branches: vec![branch],
            aggregate_result_limit: None,
        }
    }

    /// Compose independent live queries into ONE canonical declaration.
    ///
    /// Inputs flatten: a nested union contributes its branches, never a
    /// sub-tree. Duplicates collapse and order is canonicalized, so
    /// permutations of the same inputs are the same value.
    /// `aggregate_result_limit` bounds the merged row union globally.
    pub fn union(
        branches: impl IntoIterator<Item = LiveQuery>,
        aggregate_result_limit: Option<usize>,
    ) -> Result<Self, LiveQueryError> {
        let mut canonical = BTreeSet::new();
        for branch in branches {
            if branch.aggregate_result_limit.is_some() {
                return Err(LiveQueryError::NestedAggregateResultLimit);
            }
            canonical.extend(branch.branches);
        }
        if canonical.is_empty() {
            return Err(LiveQueryError::EmptyUnion);
        }
        if aggregate_result_limit == Some(0) {
            return Err(LiveQueryError::AggregateResultLimitZero);
        }
        if canonical.len() > Self::MAX_BRANCHES {
            return Err(LiveQueryError::TooManyQueryBranches {
                requested: canonical.len(),
                maximum: Self::MAX_BRANCHES,
            });
        }
        Ok(Self {
            branches: canonical.into_iter().collect(),
            aggregate_result_limit,
        })
    }

    /// The canonical branches, in the one order every surface reports and
    /// every frame's per-branch evidence is indexed by. Never empty.
    #[must_use]
    pub fn branches(&self) -> &[Demand] {
        &self.branches
    }

    /// The declared bound on the MERGED row union, applied after branch rows
    /// are unioned by event id. `None` is unbounded.
    #[must_use]
    pub const fn aggregate_result_limit(&self) -> Option<usize> {
        self.aggregate_result_limit
    }
}

