//! [`LiveQuery`] — the app-facing read declaration (#1108).
//!
//! One live query is one or more complete, independent [`Demand`] branches
//! observed through ONE lifecycle. `Demand` itself stays atomic: it owns a
//! selection, a source authority, an access context, a cache mode and a
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::hash::{DefaultHasher, Hash, Hasher};

    use super::*;
    use crate::binding::Filter;
    use crate::descriptor::{AccessContext, ReadRouting};

    fn demand(kind: u16) -> Demand {
        Demand {
            selection: Filter {
                kinds: Some(BTreeSet::from([kind])),
                ..Filter::default()
            },
            ..Demand::default()
        }
    }

    fn hash(value: &LiveQuery) -> u64 {
        let mut hasher = DefaultHasher::new();
        value.hash(&mut hasher);
        hasher.finish()
    }

    #[test]
    fn a_single_query_is_one_branch_with_no_aggregate_bound() {
        let query = LiveQuery::single(demand(1));
        assert_eq!(query.branches(), [demand(1)]);
        assert_eq!(query.aggregate_result_limit(), None);
    }

    #[test]
    fn permutations_nesting_and_duplicates_share_one_value_and_hash() {
        let flat = LiveQuery::union(
            [
                LiveQuery::single(demand(1)),
                LiveQuery::single(demand(7)),
                LiveQuery::single(demand(9735)),
            ],
            Some(9),
        )
        .unwrap();
        let nested = LiveQuery::union(
            [
                LiveQuery::single(demand(9735)),
                LiveQuery::union(
                    [
                        LiveQuery::single(demand(7)),
                        LiveQuery::single(demand(1)),
                        LiveQuery::single(demand(7)),
                    ],
                    None,
                )
                .unwrap(),
            ],
            Some(9),
        )
        .unwrap();

        assert_eq!(flat, nested);
        assert_eq!(hash(&flat), hash(&nested));
        assert_eq!(flat.branches(), [demand(1), demand(7), demand(9735)]);
    }

    #[test]
    fn policy_distinct_branches_are_not_collapsed() {
        let mut cache_only = demand(1);
        cache_only.freshness = crate::descriptor::Freshness::CacheOnly;
        let query = LiveQuery::union(
            [
                LiveQuery::single(demand(1)),
                LiveQuery::single(cache_only.clone()),
            ],
            None,
        )
        .unwrap();
        let mut expected = vec![demand(1), cache_only];
        expected.sort();
        assert_eq!(query.branches(), expected);
    }

    #[test]
    fn equal_selections_with_different_source_or_access_remain_distinct() {
        let relay = nostr::RelayUrl::parse("wss://a.example").unwrap();
        let pinned = Demand::new(
            demand(1).selection,
            ReadRouting::Explicit(vec![relay]),
            AccessContext::Public,
        )
        .unwrap();
        let query = LiveQuery::union(
            [LiveQuery::single(demand(1)), LiveQuery::single(pinned)],
            None,
        )
        .unwrap();
        assert_eq!(query.branches().len(), 2);
    }

    #[test]
    fn every_unconstructible_declaration_is_a_typed_refusal() {
        assert_eq!(
            LiveQuery::union(Vec::new(), None),
            Err(LiveQueryError::EmptyUnion)
        );
        assert_eq!(
            LiveQuery::union([LiveQuery::single(demand(1))], Some(0)),
            Err(LiveQueryError::AggregateResultLimitZero)
        );
        assert_eq!(
            LiveQuery::union(
                [LiveQuery::union([LiveQuery::single(demand(1))], Some(3)).unwrap()],
                None
            ),
            Err(LiveQueryError::NestedAggregateResultLimit)
        );
        let over_cap = (0..=LiveQuery::MAX_BRANCHES)
            .map(|index| LiveQuery::single(demand(index as u16)))
            .collect::<Vec<_>>();
        assert_eq!(
            LiveQuery::union(over_cap, None),
            Err(LiveQueryError::TooManyQueryBranches {
                requested: LiveQuery::MAX_BRANCHES + 1,
                maximum: LiveQuery::MAX_BRANCHES,
            })
        );
    }

    /// The ceiling bounds the branches an observation actually opens, so it is
    /// counted AFTER duplicates collapse. Counting the caller's input list
    /// instead would refuse a legal declaration: composing two overlapping
    /// queries repeats their shared branches in the input while the canonical
    /// set stays inside the ceiling.
    #[test]
    fn the_branch_ceiling_counts_the_canonical_set_not_the_input_list() {
        let mut inputs = (0..LiveQuery::MAX_BRANCHES)
            .map(|index| LiveQuery::single(demand(index as u16)))
            .collect::<Vec<_>>();
        inputs.push(LiveQuery::single(demand(0)));
        assert_eq!(inputs.len(), LiveQuery::MAX_BRANCHES + 1);

        let query = LiveQuery::union(inputs, None)
            .expect("a repeated branch is not an extra branch against the ceiling");
        assert_eq!(query.branches().len(), LiveQuery::MAX_BRANCHES);
    }

    #[test]
    fn the_aggregate_bound_participates_in_value_identity() {
        let branch = LiveQuery::single(demand(1));
        let three = LiveQuery::union([branch.clone()], Some(3)).unwrap();
        let five = LiveQuery::union([branch.clone()], Some(5)).unwrap();
        let unlimited = LiveQuery::union([branch], None).unwrap();
        assert_ne!(three, five);
        assert_ne!(three, unlimited);
        assert_ne!(hash(&three), hash(&five));
    }
}
