//! Per-query coverage aggregation — ruling §6 ("when is a query
//! `CompleteUpTo(T)`?"). A pure function of a handle's current atoms, the
//! router's compiled plan (for each atom's CURRENT covering relay set), and
//! the store's watermarks. No mutation, no attribution bookkeeping — that
//! lives in `attribution.rs`; this module only READS what has already been
//! recorded.

use std::collections::BTreeSet;

use nmp_grammar::ConcreteFilter;
use nmp_router::RelayPlan;
use nmp_store::{coverage_key, EventStore};
use nostr::Timestamp;

/// The query-level coverage summary the ruling defines (§6): `Unknown`
/// unless EVERY atom is proven at EVERY relay in its current covering set
/// (unanimity, not 1-of-k — ruled explicitly: a lagging relay must not be
/// read as authoritative-empty).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryCoverage {
    CompleteUpTo(Timestamp),
    Unknown,
}

/// Compute `atoms`' aggregate [`QueryCoverage`] against `plan` (for each
/// atom's current covering relay set — the relays whose compiled `WireReq`
/// absorbs that atom's key) and `store` (for each `(atom, relay)`'s proven
/// interval).
///
/// - An atom with an EMPTY covering set (no relay's current wire filter
///   absorbs it — e.g. `NoCandidates` shortfall) makes the whole query
///   `Unknown`.
/// - An atom is proven at a relay iff a coverage row exists there with
///   `from <= atom.since.unwrap_or(0)` (the query's own window floor); its
///   contribution is that row's `through`.
/// - The atom's own watermark is the MINIMUM `through` over its covering
///   set; the query's watermark is the MINIMUM over every atom's watermark.
///   `Unknown` propagates as soon as any atom/relay pair is unproven —
///   never partially aggregated.
pub(crate) fn query_coverage<S: EventStore>(
    atoms: &BTreeSet<ConcreteFilter>,
    plan: &RelayPlan,
    store: &S,
) -> QueryCoverage {
    if atoms.is_empty() {
        return QueryCoverage::Unknown;
    }

    let mut query_watermark: Option<Timestamp> = None;

    for atom in atoms {
        let key = coverage_key(atom);
        let covering: BTreeSet<&nostr::RelayUrl> = plan
            .reqs
            .iter()
            .filter_map(|(relay, reqs)| {
                reqs.iter()
                    .any(|r| r.absorbed.contains(&key))
                    .then_some(relay)
            })
            .collect();

        if covering.is_empty() {
            return QueryCoverage::Unknown;
        }

        let window_start = Timestamp::from(atom.since.unwrap_or(0));
        let mut atom_watermark: Option<Timestamp> = None;
        for relay in &covering {
            match store.get_coverage(key, relay) {
                Some(interval) if interval.from <= window_start => {
                    atom_watermark = Some(match atom_watermark {
                        None => interval.through,
                        Some(cur) => cur.min(interval.through),
                    });
                }
                _ => return QueryCoverage::Unknown,
            }
        }

        let Some(t) = atom_watermark else {
            return QueryCoverage::Unknown;
        };
        query_watermark = Some(match query_watermark {
            None => t,
            Some(cur) => cur.min(t),
        });
    }

    match query_watermark {
        Some(t) => QueryCoverage::CompleteUpTo(t),
        None => QueryCoverage::Unknown,
    }
}
