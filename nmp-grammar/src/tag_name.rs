//! [`TagName`] — a single-letter Nostr tag name, parameterized rather than
//! spelled out as per-tag enum variants (VISION §2 P2: `Tag(char)`, closed
//! and introspectable, never an app closure).

/// A single-letter Nostr tag name.
///
/// M1's valid set is `p, e, a, d, E, t, q` (validated at construction via
/// [`TagName::new`]). Note `e` and `E` are *distinct* tag names (lowercase
/// "referenced event", uppercase "root event" per NIP-10-style conventions) —
/// [`TagName`] preserves case rather than folding it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TagName(char);

/// The closed set of single-letter tag names M1's grammar can bind.
const VALID: [char; 7] = ['p', 'e', 'a', 'd', 'E', 't', 'q'];

impl TagName {
    /// Construct a [`TagName`], validating `c` against the closed M1 set
    /// (`p, e, a, d, E, t, q`). Returns `None` for any other character.
    pub fn new(c: char) -> Option<Self> {
        if VALID.contains(&c) {
            Some(Self(c))
        } else {
            None
        }
    }

    /// The underlying character.
    pub fn as_char(&self) -> char {
        self.0
    }
}

impl std::fmt::Display for TagName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_the_closed_m1_set() {
        for c in VALID {
            assert!(TagName::new(c).is_some(), "expected {c:?} to be valid");
        }
    }

    #[test]
    fn rejects_anything_outside_the_closed_set() {
        for c in ['x', 'z', 'P', 'D', 'A', '1', ' ', 'é'] {
            assert!(TagName::new(c).is_none(), "expected {c:?} to be rejected");
        }
    }

    #[test]
    fn lowercase_e_and_uppercase_e_are_distinct() {
        let e = TagName::new('e').unwrap();
        let big_e = TagName::new('E').unwrap();
        assert_ne!(e, big_e);
        assert_eq!(e.as_char(), 'e');
        assert_eq!(big_e.as_char(), 'E');
    }

    #[test]
    fn ordering_is_total_and_deterministic() {
        let mut names: Vec<TagName> = VALID.iter().map(|&c| TagName::new(c).unwrap()).collect();
        names.sort();
        let mut chars: Vec<char> = names.iter().map(|t| t.as_char()).collect();
        let mut expected = VALID.to_vec();
        expected.sort();
        assert_eq!(chars, expected);
        // Sorting twice is a no-op (total order, no panics on Ord impl).
        chars.sort();
        assert_eq!(chars, expected);
    }
}
