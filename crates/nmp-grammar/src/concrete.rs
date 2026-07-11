//! [`ConcreteFilter`] — a fully-resolved filter (no bindings), the unit of
//! the demand set and the refcount/dedup key, plus [`DescriptorHash`], its
//! canonical hash.

use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};

use crate::tag_name::TagName;

/// A fully-resolved filter — NO bindings. The unit of the demand set and
/// refcount/dedup key.
///
/// Every field is co-pinned: for a coordinate-derived atom (see M1 plan
/// §3.5), `kinds`/`authors`/`#d` are singletons TOGETHER, not independent
/// field-sets.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConcreteFilter {
    /// Literal kind set.
    pub kinds: Option<BTreeSet<u16>>,
    /// Resolved author hex-pubkey set.
    pub authors: Option<BTreeSet<String>>,
    /// Resolved event-id hex set.
    pub ids: Option<BTreeSet<String>>,
    /// Resolved per-tag value sets.
    pub tags: BTreeMap<TagName, BTreeSet<String>>,
    /// Inclusive lower bound on `created_at`.
    pub since: Option<u64>,
    /// Inclusive upper bound on `created_at`.
    pub until: Option<u64>,
    /// Result-count cap.
    pub limit: Option<usize>,
}

/// A canonical, stable hash of a [`ConcreteFilter`] — the demand/refcount
/// key. Deterministic across process runs (unlike `std::collections::HashMap`'s
/// default `RandomState`, which reseeds per-process): `ConcreteFilter`'s
/// fields are already canonical (`BTreeSet`/`BTreeMap` normalize member
/// order regardless of insertion order), and hashing runs through a
/// fixed-seed FNV-1a [`std::hash::Hasher`] rather than a randomized one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DescriptorHash(u64);

impl DescriptorHash {
    /// The raw `u64` hash value.
    pub fn as_u64(&self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for DescriptorHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

/// A fixed-seed FNV-1a hasher. Chosen over `std`'s default `SipHasher`
/// (reachable only via the randomized `RandomState`) specifically because
/// its seed is a constant, not a per-process random value — required for
/// `DescriptorHash` to be stable across runs and processes, which is the
/// whole point of using it as a durable demand/refcount key.
struct StableHasher(u64);

const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

impl StableHasher {
    fn new() -> Self {
        Self(FNV_OFFSET_BASIS)
    }
}

impl Hasher for StableHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= b as u64;
            self.0 = self.0.wrapping_mul(FNV_PRIME);
        }
    }
}

impl ConcreteFilter {
    /// Lower to `nostr::Filter` at the resolver/store boundary.
    ///
    /// # Panics
    /// Panics if `authors`/`ids` contain a string that isn't a valid
    /// 32-byte-hex pubkey/event-id, or if a tag key somehow isn't one of
    /// M1's valid single-letter tags. Both are construction invariants of
    /// `ConcreteFilter` (its hex strings always originate from
    /// `PublicKey::to_hex`/`EventId::to_hex` round-trips, and its tag keys
    /// are always `TagName`s, which are pre-validated) — a panic here means
    /// a genuine invariant violation upstream, not a reachable user input
    /// error, so it is not silently swallowed.
    pub fn to_nostr(&self) -> nostr::Filter {
        let mut f = nostr::Filter::new();

        if let Some(kinds) = &self.kinds {
            f = f.kinds(kinds.iter().map(|&k| nostr::Kind::from(k)));
        }

        if let Some(authors) = &self.authors {
            let parsed: Vec<nostr::PublicKey> = authors
                .iter()
                .map(|hex| {
                    nostr::PublicKey::from_hex(hex)
                        .unwrap_or_else(|e| panic!("ConcreteFilter authors invariant violated: {hex:?} is not a valid hex pubkey: {e}"))
                })
                .collect();
            f = f.authors(parsed);
        }

        if let Some(ids) = &self.ids {
            let parsed: Vec<nostr::EventId> = ids
                .iter()
                .map(|hex| {
                    nostr::EventId::from_hex(hex)
                        .unwrap_or_else(|e| panic!("ConcreteFilter ids invariant violated: {hex:?} is not a valid hex event id: {e}"))
                })
                .collect();
            f = f.ids(parsed);
        }

        for (tag, values) in &self.tags {
            let single_letter = nostr::SingleLetterTag::from_char(tag.as_char())
                .unwrap_or_else(|e| panic!("TagName {tag} invariant violated: {e}"));
            f = f.custom_tags(single_letter, values.iter().cloned());
        }

        if let Some(since) = self.since {
            f = f.since(nostr::Timestamp::from(since));
        }
        if let Some(until) = self.until {
            f = f.until(nostr::Timestamp::from(until));
        }
        if let Some(limit) = self.limit {
            f = f.limit(limit);
        }

        f
    }

    /// Canonical, stable hash — the demand/refcount key. Two `ConcreteFilter`
    /// values built from the same logical set of fields but assembled by
    /// inserting elements into their `BTreeSet`/`BTreeMap` fields in a
    /// different order hash identically (`BTreeSet`/`BTreeMap` are already
    /// order-normalizing; this hash adds run-to-run stability on top).
    pub fn hash(&self) -> DescriptorHash {
        let mut hasher = StableHasher::new();
        Hash::hash(self, &mut hasher);
        DescriptorHash(hasher.finish())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cf(authors: Vec<&str>, tags_d: Vec<&str>) -> ConcreteFilter {
        ConcreteFilter {
            kinds: Some(BTreeSet::from([1u16])),
            authors: Some(authors.into_iter().map(String::from).collect()),
            ids: None,
            tags: {
                let mut m = BTreeMap::new();
                if !tags_d.is_empty() {
                    m.insert(
                        TagName::new('d').unwrap(),
                        tags_d.into_iter().map(String::from).collect(),
                    );
                }
                m
            },
            since: Some(100),
            until: None,
            limit: Some(50),
        }
    }

    #[test]
    fn hash_is_stable_and_canonical_regardless_of_insertion_order() {
        let a = cf(vec!["aa", "bb", "cc"], vec!["g1", "g2"]);
        let b = cf(vec!["cc", "aa", "bb"], vec!["g2", "g1"]);
        assert_eq!(a, b, "BTreeSet/BTreeMap normalize order already");
        assert_eq!(a.hash(), b.hash());

        // Same value hashed twice (simulating two separate process-local
        // calls) is stable — not merely equal-because-cached.
        assert_eq!(a.hash(), a.hash());
    }

    #[test]
    fn hash_differs_for_logically_different_filters() {
        let a = cf(vec!["aa", "bb", "cc"], vec![]);
        let b = cf(vec!["aa", "bb", "dd"], vec![]);
        assert_ne!(a.hash(), b.hash());
    }

    #[test]
    fn to_nostr_lowers_kinds_authors_tags_since_until_limit() {
        let pk = "a".repeat(64);
        let cf = ConcreteFilter {
            kinds: Some(BTreeSet::from([1u16, 3u16])),
            authors: Some(BTreeSet::from([pk.clone()])),
            ids: None,
            tags: {
                let mut m = BTreeMap::new();
                m.insert(
                    TagName::new('d').unwrap(),
                    BTreeSet::from(["g1".to_string()]),
                );
                m
            },
            since: Some(100),
            until: Some(200),
            limit: Some(10),
        };

        let nf = cf.to_nostr();

        assert_eq!(
            nf.kinds,
            Some(BTreeSet::from([
                nostr::Kind::from(1u16),
                nostr::Kind::from(3u16)
            ]))
        );
        assert_eq!(
            nf.authors,
            Some(BTreeSet::from([nostr::PublicKey::from_hex(&pk).unwrap()]))
        );
        assert_eq!(nf.since, Some(nostr::Timestamp::from(100u64)));
        assert_eq!(nf.until, Some(nostr::Timestamp::from(200u64)));
        assert_eq!(nf.limit, Some(10));

        let d_tag = nostr::SingleLetterTag::from_char('d').unwrap();
        assert_eq!(
            nf.generic_tags.get(&d_tag),
            Some(&BTreeSet::from(["g1".to_string()]))
        );
    }

    #[test]
    fn to_nostr_lowered_filter_matches_events_via_match_event() {
        use nostr::filter::MatchEventOptions;
        use nostr::{EventBuilder, Keys, Kind};

        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::TextNote, "hello")
            .sign_with_keys(&keys)
            .unwrap();

        let matching = ConcreteFilter {
            kinds: Some(BTreeSet::from([1u16])),
            authors: Some(BTreeSet::from([keys.public_key().to_hex()])),
            ..ConcreteFilter::default()
        };
        assert!(matching
            .to_nostr()
            .match_event(&event, MatchEventOptions::new()));

        let non_matching = ConcreteFilter {
            kinds: Some(BTreeSet::from([1u16])),
            authors: Some(BTreeSet::from([Keys::generate().public_key().to_hex()])),
            ..ConcreteFilter::default()
        };
        assert!(!non_matching
            .to_nostr()
            .match_event(&event, MatchEventOptions::new()));
    }
}
