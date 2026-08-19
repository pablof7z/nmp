//! The 2-relay-min + cap coverage solver: a capped, greedy, deterministic
//! k-cover approximation (M2 plan §3). Set-cover is NP-hard, so greedy with
//! a deterministic tiebreak is the correct minimum-shape choice — not an
//! attempt at an exact/optimal solution.

use std::collections::{BTreeMap, BTreeSet};

use crate::facts::{LanedRelay, PublicKey, RelayUrl};

/// Input: authors, each with an ordered candidate relay list (lane-priority
/// order, `build_candidates`) — authoritative outbound facts plus projected
/// hint/provenance evidence. Operator policy is applied outside the solve.
pub struct CoverageInput {
    pub candidates: BTreeMap<PublicKey, Vec<LanedRelay>>,
    pub k: usize,
    pub cap: usize,
}

/// Output: the covering relay set + per-author assignment + shortfall
/// report.
#[derive(Debug, Clone, Default)]
pub struct Coverage {
    /// Never an accumulated union; `|selected| <= cap` always.
    pub selected: BTreeSet<RelayUrl>,
    /// Each author -> the relays covering it.
    pub assignment: BTreeMap<PublicKey, BTreeSet<RelayUrl>>,
    /// Authors that did not reach `k`.
    pub shortfall: BTreeMap<PublicKey, Shortfall>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Shortfall {
    pub requested_k: usize,
    pub achieved: usize,
    pub reason: ShortfallReason,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShortfallReason {
    /// The author has no candidate relays at all.
    NoCandidates,
    /// The author lists fewer than `k` relays — `achieved` is their
    /// ceiling, NOT a defect.
    FewerCandidatesThanK,
    /// The global fan-out cap was hit before this author reached `k`.
    CapExhausted,
}

/// Greedy, deterministic capped k-cover (M2 plan §3).
pub fn solve(input: &CoverageInput) -> Coverage {
    let mut selected: BTreeSet<RelayUrl> = BTreeSet::new();
    let mut assignment: BTreeMap<PublicKey, BTreeSet<RelayUrl>> = input
        .candidates
        .keys()
        .map(|a| (*a, BTreeSet::new()))
        .collect();

    // Each author's ceiling clamps k to however many distinct relays they
    // actually list.
    let ceilings: BTreeMap<PublicKey, usize> = input
        .candidates
        .iter()
        .map(|(a, list)| {
            let distinct: BTreeSet<&RelayUrl> = list.iter().map(|lr| &lr.url).collect();
            (*a, distinct.len().min(input.k))
        })
        .collect();

    let mut need: BTreeMap<PublicKey, usize> = ceilings.clone();

    loop {
        if selected.len() >= input.cap {
            break;
        }
        if need.values().all(|&n| n == 0) {
            break;
        }

        // Score every not-yet-selected candidate relay by how many
        // still-needed (author, open-slot) pairs it would fill.
        let mut scores: BTreeMap<RelayUrl, usize> = BTreeMap::new();
        for (author, list) in &input.candidates {
            if need.get(author).cloned().unwrap_or(0) == 0 {
                continue;
            }
            let already_covered = &assignment[author];
            let mut seen = BTreeSet::new();
            for lr in list {
                if already_covered.contains(&lr.url) || selected.contains(&lr.url) {
                    continue;
                }
                if seen.insert(lr.url.clone()) {
                    *scores.entry(lr.url.clone()).or_insert(0) += 1;
                }
            }
        }

        let Some((best_relay, _)) =
            scores
                .into_iter()
                .max_by(|(a_url, a_score), (b_url, b_score)| {
                    // Highest score wins; ties broken by lexicographic RelayUrl
                    // (determinism -> reproducible plans -> stable diffs).
                    a_score.cmp(b_score).then_with(|| b_url.cmp(a_url)) // reverse: prefer smaller url on tie
                })
        else {
            // No candidate relay can fill any remaining open slot.
            break;
        };

        selected.insert(best_relay.clone());
        for (author, list) in &input.candidates {
            if need.get(author).cloned().unwrap_or(0) == 0 {
                continue;
            }
            if list.iter().any(|lr| lr.url == best_relay) {
                assignment
                    .get_mut(author)
                    .unwrap()
                    .insert(best_relay.clone());
                let n = need.get_mut(author).unwrap();
                *n = n.saturating_sub(1);
            }
        }
    }

    // Shortfall is reported against the ORIGINAL requested `k`, not the
    // per-author clamped ceiling: an author with fewer than `k` candidate
    // relays reaches its ceiling (an empty `need`) without ever reaching
    // `k` itself, and that gap must still be visible (test 5:
    // `FewerCandidatesThanK` is reported even though it isn't a defect).
    let mut shortfall = BTreeMap::new();
    for (author, assigned) in &assignment {
        let achieved = assigned.len();
        if achieved >= input.k {
            continue;
        }
        let candidate_count: BTreeSet<&RelayUrl> =
            input.candidates[author].iter().map(|lr| &lr.url).collect();
        let reason = if candidate_count.is_empty() {
            ShortfallReason::NoCandidates
        } else if candidate_count.len() < input.k {
            ShortfallReason::FewerCandidatesThanK
        } else {
            ShortfallReason::CapExhausted
        };
        shortfall.insert(
            *author,
            Shortfall {
                requested_k: input.k,
                achieved,
                reason,
            },
        );
    }

    Coverage {
        selected,
        assignment,
        shortfall,
    }
}

