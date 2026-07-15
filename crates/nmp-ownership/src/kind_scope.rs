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

    /// One concrete kind contained in BOTH `self` and `other`, or `None` if
    /// the two scopes are disjoint -- the mechanism `overlaps` is built on,
    /// and the same witness the `nmp-audit` layer-2 audit (`ClaimSet`) names
    /// in its `ClaimOverlap` error (routing-and-ownership.md §4.2 layer 2,
    /// legacy `NMP-OWNERSHIP-COLLISION` map minus the linker).
    ///
    /// Handles all 6 variant pairings, with the same inverted/empty-`Range`
    /// and empty-`Set` care `overlaps` used to duplicate: an empty range or
    /// an empty set intersects nothing. `Range`/`Range` returns
    /// `max(a.start(), b.start())` (the earliest kind both ranges reach,
    /// once they're known to overlap). `Set`-involving pairings return the
    /// first matching element in the set's own slice order (deterministic,
    /// not "smallest" -- the slice is not required to be sorted).
    pub fn intersection_witness(&self, other: &KindScope) -> Option<u16> {
        match (self, other) {
            (KindScope::Kind(a), KindScope::Kind(b)) => (a == b).then_some(*a),
            (KindScope::Kind(k), KindScope::Range(r))
            | (KindScope::Range(r), KindScope::Kind(k)) => r.contains(k).then_some(*k),
            (KindScope::Kind(k), KindScope::Set(s)) | (KindScope::Set(s), KindScope::Kind(k)) => {
                s.contains(k).then_some(*k)
            }
            (KindScope::Range(a), KindScope::Range(b)) => {
                // `RangeInclusive` doesn't normalize an inverted (empty)
                // range -- `10..=5`.contains(x) is always false, but the
                // naive `start <= other_end && other_start <= end` test
                // below would still (wrongly) report an empty range as
                // overlapping anything it's numerically nested inside.
                if a.is_empty() || b.is_empty() {
                    None
                } else if a.start() <= b.end() && b.start() <= a.end() {
                    Some((*a.start()).max(*b.start()))
                } else {
                    None
                }
            }
            (KindScope::Range(r), KindScope::Set(s)) | (KindScope::Set(s), KindScope::Range(r)) => {
                if r.is_empty() {
                    None
                } else {
                    s.iter().find(|k| r.contains(k)).copied()
                }
            }
            (KindScope::Set(a), KindScope::Set(b)) => a.iter().find(|k| b.contains(k)).copied(),
        }
    }

    /// Whether this scope shares at least one kind with `other` -- the
    /// exclusivity check the Unit G workspace audit runs pairwise across
    /// every linked (and unlinked, per §4.2) module's claims. Symmetric:
    /// `a.overlaps(b) == b.overlaps(a)`. Reimplemented on top of
    /// [`KindScope::intersection_witness`] so the empty-range/empty-set
    /// care lives in exactly one place.
    pub fn overlaps(&self, other: &KindScope) -> bool {
        self.intersection_witness(other).is_some()
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

    // `10..=5` below is a deliberate boundary case (an inverted/empty
    // `KindScope::Range`), not a mistake.
    #[allow(clippy::reversed_empty_ranges)]
    #[test]
    fn intersection_witness_every_variant_pairing() {
        // Kind vs Kind: the shared kind itself, or nothing.
        assert_eq!(
            KindScope::Kind(7).intersection_witness(&KindScope::Kind(7)),
            Some(7)
        );
        assert_eq!(
            KindScope::Kind(7).intersection_witness(&KindScope::Kind(8)),
            None
        );

        // Kind vs Range, both directions: the kind, if it's in range.
        let range = KindScope::Range(9000..=9030);
        assert_eq!(
            KindScope::Kind(9010).intersection_witness(&range),
            Some(9010)
        );
        assert_eq!(
            range.intersection_witness(&KindScope::Kind(9010)),
            Some(9010)
        );
        assert_eq!(KindScope::Kind(8999).intersection_witness(&range), None);
        assert_eq!(range.intersection_witness(&KindScope::Kind(8999)), None);

        // Kind vs Set, both directions: the kind, if it's a member.
        let set = KindScope::Set(&[1059, 13, 14, 15, 10050]);
        assert_eq!(KindScope::Kind(13).intersection_witness(&set), Some(13));
        assert_eq!(set.intersection_witness(&KindScope::Kind(13)), Some(13));
        assert_eq!(KindScope::Kind(16).intersection_witness(&set), None);
        assert_eq!(set.intersection_witness(&KindScope::Kind(16)), None);

        // Range vs Range: max(start, start) once overlap is established;
        // touching at the boundary is still an overlap; disjoint is None;
        // an inverted (empty) range on either side is always None, even
        // when numerically nested inside the other.
        let a = KindScope::Range(9000..=9030);
        let b = KindScope::Range(9020..=9050);
        assert_eq!(a.intersection_witness(&b), Some(9020));
        assert_eq!(b.intersection_witness(&a), Some(9020));
        let touching = KindScope::Range(9030..=9040);
        assert_eq!(a.intersection_witness(&touching), Some(9030));
        let disjoint = KindScope::Range(39000..=39009);
        assert_eq!(a.intersection_witness(&disjoint), None);
        let empty_range = KindScope::Range(10..=5);
        assert_eq!(
            empty_range.intersection_witness(&KindScope::Range(0..=100)),
            None
        );
        assert_eq!(
            KindScope::Range(0..=100).intersection_witness(&empty_range),
            None
        );

        // Range vs Set, both directions: the first set element (in slice
        // order) that falls in the range; an empty range yields None even
        // when the set is nonempty.
        let small_range = KindScope::Range(1050..=1060);
        let small_set = KindScope::Set(&[1059]);
        assert_eq!(small_range.intersection_witness(&small_set), Some(1059));
        assert_eq!(small_set.intersection_witness(&small_range), Some(1059));
        assert_eq!(
            KindScope::Range(1..=10).intersection_witness(&small_set),
            None
        );
        assert_eq!(
            empty_range.intersection_witness(&KindScope::Set(&[7])),
            None
        );
        assert_eq!(
            KindScope::Set(&[7]).intersection_witness(&empty_range),
            None
        );
        // Deterministic first-match order: 13 precedes 14 in the slice.
        let ordered_set = KindScope::Set(&[13, 14]);
        assert_eq!(
            KindScope::Range(13..=14).intersection_witness(&ordered_set),
            Some(13)
        );

        // Set vs Set: the first element of `self`'s slice found in
        // `other`; disjoint sets yield None; an empty set yields None.
        assert_eq!(
            KindScope::Set(&[1059, 13]).intersection_witness(&KindScope::Set(&[13, 14])),
            Some(13)
        );
        assert_eq!(
            KindScope::Set(&[1059, 13]).intersection_witness(&KindScope::Set(&[14, 15])),
            None
        );
        let empty_set = KindScope::Set(&[]);
        assert_eq!(empty_set.intersection_witness(&KindScope::Set(&[0])), None);
        assert_eq!(KindScope::Set(&[0]).intersection_witness(&empty_set), None);

        // `overlaps` is exactly `intersection_witness(..).is_some()`.
        assert_eq!(a.overlaps(&b), a.intersection_witness(&b).is_some());
        assert_eq!(
            empty_range.overlaps(&KindScope::Range(0..=100)),
            empty_range
                .intersection_witness(&KindScope::Range(0..=100))
                .is_some()
        );
    }
}
