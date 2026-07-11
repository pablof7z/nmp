//! [`Selector`] and [`IdentityField`] — the closed projection vocabulary and
//! the reactive identity root (VISION §2 P2, P3).

/// The CLOSED, introspectable projection vocabulary a [`crate::Derived`]
/// binding projects an inner filter's results through. Never an app closure
/// (VISION §2 P2) — a use case outside this vocabulary extends the
/// vocabulary; it never admits app code.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Selector {
    /// Project each matched event's author pubkey.
    Authors,
    /// Project each matched event's id.
    Ids,
    /// Project each value of an arbitrary event tag, keyed by its exact tag
    /// name (parameterized, not per-tag variants). This projects over
    /// already-acquired events (a purely local read), so it is NOT
    /// restricted to [`crate::IndexedTagName`]'s single-letter wire syntax —
    /// `alt`, the NIP-70 `-` tag, or any other multi-character/punctuation
    /// tag name an event actually carries is a legal key here. Case and
    /// spelling are matched exactly (`"e"` and `"E"` are distinct keys, same
    /// as `"e"` and `"ee"` are).
    Tag(String),
    /// Project the `a`-coordinate(s): `(kind, author, d)`. CO-PINNED — see
    /// M1 plan §3.5; a coordinate-projecting `Derived` fans out into one
    /// co-pinned atom per coordinate rather than factoring into independent
    /// kinds/authors/#d field-sets.
    AddressCoord,
}

/// The reactive identity root. The app sets it; the engine reacts
/// (VISION §2 P3). Extensible — do not treat this as a closed set to forbid
/// growing (e.g. a future `ActiveRelayList`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IdentityField {
    /// The currently active signer's public key, or unset.
    ActivePubkey,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selector_variants_are_ord_and_hash() {
        use std::collections::BTreeSet;
        let mut set: BTreeSet<Selector> = BTreeSet::new();
        set.insert(Selector::Authors);
        set.insert(Selector::Ids);
        set.insert(Selector::Tag("p".to_string()));
        set.insert(Selector::AddressCoord);
        assert_eq!(set.len(), 4);
    }

    #[test]
    fn tag_selector_distinguishes_by_tag_name() {
        let p = Selector::Tag("p".to_string());
        let d = Selector::Tag("d".to_string());
        assert_ne!(p, d);
    }

    /// `Selector::Tag` projects arbitrary, multi-character event-tag names —
    /// not just the single-letter wire-filter alphabet (#64).
    #[test]
    fn tag_selector_accepts_arbitrary_multi_character_names() {
        for name in ["-", "poop", "alt"] {
            let selector = Selector::Tag(name.to_string());
            assert_eq!(selector, Selector::Tag(name.to_string()));
        }
    }

    #[test]
    fn identity_field_is_ord_and_hash() {
        use std::collections::BTreeSet;
        let mut set = BTreeSet::new();
        set.insert(IdentityField::ActivePubkey);
        assert!(set.contains(&IdentityField::ActivePubkey));
    }
}
