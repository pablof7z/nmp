//! [`IndexedTagName`] — a single case-sensitive ASCII-letter Nostr tag name,
//! parameterized rather than spelled out as per-tag enum variants (VISION §2
//! P2: `Tag(char)`, closed and introspectable, never an app closure).
//!
//! This is the wire/local **indexed filter** vocabulary only: NIP-01 defines
//! generic relay/local filter keys (`Filter.tags`, `#<letter>` queries) as
//! exactly one ASCII letter, `a`-`z` or `A`-`Z`, because those are the tags
//! relays are expected to index. All 52 letters are structurally valid —
//! there is no hand-picked subset (#64): every standards-defined single-
//! letter tag is a filter key without a core grammar change, and adding a new
//! one is not a grammar change either. Arbitrary multi-character event-tag
//! names (`alt`, the NIP-70 `-` tag, …) are a DIFFERENT concept — valid event
//! data that can never be an indexed filter key; see [`crate::Selector::Tag`],
//! which carries a plain `String` for exactly that reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IndexedTagName(char);

impl IndexedTagName {
    /// Construct an [`IndexedTagName`], validating `c` is exactly one ASCII
    /// letter (`a`-`z` or `A`-`Z`). Returns `None` for anything else —
    /// digits, punctuation, non-ASCII. Case is preserved: `'e'` and `'E'` are
    /// distinct indexed tag names (lowercase "referenced event", uppercase
    /// "root event" per NIP-10-style conventions).
    pub fn new(c: char) -> Option<Self> {
        if c.is_ascii_alphabetic() {
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

impl std::fmt::Display for IndexedTagName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// All 52 ASCII letters are valid — structural, not a hand-picked
    /// subset. `x` and `Z` in particular were NOT in the old hard-coded
    /// whitelist; their acceptance here proves this is a syntax rule, not
    /// another expanded list (#64 acceptance evidence).
    #[test]
    fn accepts_every_ascii_letter_both_cases() {
        for c in ('a'..='z').chain('A'..='Z') {
            assert!(
                IndexedTagName::new(c).is_some(),
                "expected {c:?} to be valid"
            );
        }
    }

    #[test]
    fn rejects_anything_that_is_not_a_single_ascii_letter() {
        for c in ['1', ' ', '-', 'é', '_'] {
            assert!(
                IndexedTagName::new(c).is_none(),
                "expected {c:?} to be rejected"
            );
        }
    }

    #[test]
    fn lowercase_e_and_uppercase_e_are_distinct() {
        let e = IndexedTagName::new('e').unwrap();
        let big_e = IndexedTagName::new('E').unwrap();
        assert_ne!(e, big_e);
        assert_eq!(e.as_char(), 'e');
        assert_eq!(big_e.as_char(), 'E');
    }

    #[test]
    fn ordering_is_total_and_deterministic() {
        let mut names: Vec<IndexedTagName> = ('a'..='z')
            .chain('A'..='Z')
            .map(|c| IndexedTagName::new(c).unwrap())
            .collect();
        names.sort();
        let mut chars: Vec<char> = names.iter().map(|t| t.as_char()).collect();
        let mut expected: Vec<char> = ('a'..='z').chain('A'..='Z').collect();
        expected.sort();
        assert_eq!(chars, expected);
        // Sorting twice is a no-op (total order, no panics on Ord impl).
        chars.sort();
        assert_eq!(chars, expected);
    }
}
