//! [`KindScope`] -- the compact vocabulary a [`crate::KindClaim`] uses to
//! name the kinds it owns (routing-and-ownership.md §4.1).

use std::ops::RangeInclusive;

/// A set of NIP event kinds, in one of three compact forms. `Range`/`Set`
/// kill the legacy per-kind repetition: NIP-29's `9000..=9030 ∪
/// 39000..=39009`, NIP-17's `{1059, 13, 14, 15, 10050}`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum KindScope {
    Kind(u16),
    Range(RangeInclusive<u16>),
    Set(&'static [u16]),
}

impl KindScope {
    /// Whether `kind` falls inside this scope.
    pub fn contains(&self, kind: u16) -> bool {
        match self {
            KindScope::Kind(k) => *k == kind,
            KindScope::Range(r) => r.contains(&kind),
            KindScope::Set(s) => s.contains(&kind),
        }
    }

    /// Whether this scope shares at least one kind with `other` -- the
    /// exclusivity check the Unit G workspace audit runs pairwise across
    /// every linked (and unlinked, per §4.2) module's claims. Symmetric:
    /// `a.overlaps(b) == b.overlaps(a)`.
    pub fn overlaps(&self, other: &KindScope) -> bool {
        match (self, other) {
            (KindScope::Kind(a), KindScope::Kind(b)) => a == b,
            (KindScope::Kind(k), KindScope::Range(r))
            | (KindScope::Range(r), KindScope::Kind(k)) => r.contains(k),
            (KindScope::Kind(k), KindScope::Set(s)) | (KindScope::Set(s), KindScope::Kind(k)) => {
                s.contains(k)
            }
            (KindScope::Range(a), KindScope::Range(b)) => {
                // `RangeInclusive` doesn't normalize an inverted (empty)
                // range -- `10..=5`.contains(x) is always false, but the
                // naive `start <= other_end && other_start <= end` test
                // below would still (wrongly) report an empty range as
                // overlapping anything it's numerically nested inside.
                !a.is_empty() && !b.is_empty() && a.start() <= b.end() && b.start() <= a.end()
            }
            (KindScope::Range(r), KindScope::Set(s)) | (KindScope::Set(s), KindScope::Range(r)) => {
                !r.is_empty() && s.iter().any(|k| r.contains(k))
            }
            (KindScope::Set(a), KindScope::Set(b)) => a.iter().any(|k| b.contains(k)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `10..=5` below is a deliberate boundary case (an inverted/empty
    // `KindScope::Range`), not a mistake -- clippy otherwise flags it as
    // dead code.
    #[allow(clippy::reversed_empty_ranges)]
    #[test]
    fn kindscope_contains_and_overlap() {
        // Kind: contains is exact-match only.
        let kind = KindScope::Kind(7);
        assert!(kind.contains(7));
        assert!(!kind.contains(8));

        // Range: inclusive at both boundaries.
        let range = KindScope::Range(9000..=9030);
        assert!(range.contains(9000));
        assert!(range.contains(9030));
        assert!(range.contains(9015));
        assert!(!range.contains(8999));
        assert!(!range.contains(9031));

        // Range: inverted bounds is the empty scope -- contains nothing,
        // overlaps nothing, even when numerically "inside" another range.
        let empty_range = KindScope::Range(10..=5);
        assert!(!empty_range.contains(7));
        assert!(!empty_range.overlaps(&KindScope::Range(0..=100)));
        assert!(!KindScope::Range(0..=100).overlaps(&empty_range));

        // Set: membership, incl. NIP-17's actual claim set.
        let set = KindScope::Set(&[1059, 13, 14, 15, 10050]);
        assert!(set.contains(1059));
        assert!(set.contains(10050));
        assert!(!set.contains(16));

        // Set: empty slice contains/overlaps nothing.
        let empty_set = KindScope::Set(&[]);
        assert!(!empty_set.contains(0));
        assert!(!empty_set.overlaps(&KindScope::Kind(0)));

        // Overlap: Kind vs Kind.
        assert!(KindScope::Kind(5).overlaps(&KindScope::Kind(5)));
        assert!(!KindScope::Kind(5).overlaps(&KindScope::Kind(6)));

        // Overlap: Kind vs Range, both directions (symmetry).
        assert!(KindScope::Kind(9010).overlaps(&range));
        assert!(range.overlaps(&KindScope::Kind(9010)));
        assert!(range.contains(9030) && KindScope::Kind(9030).overlaps(&range)); // boundary
        assert!(!KindScope::Kind(8999).overlaps(&range));

        // Overlap: Kind vs Set, both directions.
        let small_set = KindScope::Set(&[1059, 13]);
        assert!(KindScope::Kind(13).overlaps(&small_set));
        assert!(small_set.overlaps(&KindScope::Kind(13)));
        assert!(!KindScope::Kind(14).overlaps(&small_set));

        // Overlap: Range vs Range -- NIP-29's own two disjoint ranges
        // don't overlap each other; a range sharing exactly the boundary
        // kind does.
        let nip29_a = KindScope::Range(9000..=9030);
        let nip29_b = KindScope::Range(39000..=39009);
        assert!(!nip29_a.overlaps(&nip29_b));
        let touching_at_boundary = KindScope::Range(9030..=9040);
        assert!(nip29_a.overlaps(&touching_at_boundary));
        assert!(touching_at_boundary.overlaps(&nip29_a));

        // Overlap: Range vs Set, both directions.
        assert!(KindScope::Range(1050..=1060).overlaps(&KindScope::Set(&[1059])));
        assert!(KindScope::Set(&[1059]).overlaps(&KindScope::Range(1050..=1060)));
        assert!(!KindScope::Range(1..=10).overlaps(&KindScope::Set(&[1059])));

        // Overlap: Set vs Set.
        assert!(KindScope::Set(&[1059, 13]).overlaps(&KindScope::Set(&[13, 14])));
        assert!(!KindScope::Set(&[1059, 13]).overlaps(&KindScope::Set(&[14, 15])));
    }
}
