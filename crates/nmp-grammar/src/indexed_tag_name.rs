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

