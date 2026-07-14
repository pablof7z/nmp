//! [`ConcreteFilter`] — a fully-resolved filter (no bindings), the unit of
//! the demand set and the refcount/dedup key, plus [`DescriptorHash`], its
//! canonical hash.

use std::collections::{BTreeMap, BTreeSet};

use crate::descriptor::{AccessContext, SourceAuthority};
use crate::indexed_tag_name::IndexedTagName;

/// A relay fact carried by a projected value through a `Derived` graph.
/// This is routing input, not selection: it never changes
/// [`ConcreteFilter::to_nostr`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RoutingEvidence {
    pub relay: nostr::RelayUrl,
    pub origin: RoutingEvidenceKind,
}

/// Why a projected value may be requested from a relay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RoutingEvidenceKind {
    /// An explicit relay hint in an `e`, `a`, or `p` tag.
    Hint,
    /// The relay from which the source event carrying the value was seen.
    SourceProvenance,
}

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
    pub tags: BTreeMap<IndexedTagName, BTreeSet<String>>,
    /// Inclusive lower bound on `created_at`.
    pub since: Option<u64>,
    /// Inclusive upper bound on `created_at`.
    pub until: Option<u64>,
    /// Result-count cap.
    pub limit: Option<usize>,
}

/// A canonical, stable, COLLISION-RESISTANT hash of a [`ConcreteFilter`] —
/// the demand/refcount key, and (via `nmp-store::CoverageKey`) the durable
/// redb coverage-watermark key. Deterministic across process runs (unlike
/// `std::collections::HashMap`'s default `RandomState`, which reseeds
/// per-process).
///
/// A 256-bit BLAKE3 digest, NOT a 64-bit hash: `ConcreteFilter`'s contents
/// are network-controlled (a hostile `kind:3`/`kind:10002` steers a
/// `Binding::Derived` author set), so this value must resist DELIBERATE
/// collision construction, not just accidental clashes. A 64-bit hash
/// (the previous implementation used FNV-1a) is offline-constructible by a
/// determined attacker; the consequence for `CoverageKey` specifically is a
/// forged association between a filter and another filter's persisted
/// source evidence. BLAKE3 was chosen over
/// SHA-256 for its performance (this hash is computed on every atom
/// resolve, not just at rest) with no less cryptographic assurance for this
/// use case (content-addressing, not password hashing).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DescriptorHash([u8; 32]);

impl DescriptorHash {
    /// The raw 32-byte digest, for use as (part of) a durable storage key.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Display for DescriptorHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// Canonical byte encoding of the fields that define a [`ConcreteFilter`]'s
/// identity, fed into [`blake3::hash`]. `BTreeSet`/`BTreeMap` already
/// normalize member/key order regardless of insertion order; JSON's own
/// string quoting/escaping makes the boundary between fields unambiguous
/// (unlike naive byte concatenation without length-prefixing, which an
/// attacker could exploit to construct a collision at the FRAMING level
/// even with a strong underlying hash — e.g. `authors:["ab"], ids:["c"]`
/// colliding with `authors:["a"], ids:["bc"]`). Tag keys are rendered as
/// single-character strings rather than via `IndexedTagName`'s own (non-`Serialize`)
/// type -- no derive needed on `ConcreteFilter` itself.
fn canonical_encoding(f: &ConcreteFilter) -> Vec<u8> {
    let tags: BTreeMap<String, &BTreeSet<String>> = f
        .tags
        .iter()
        .map(|(k, v)| (k.as_char().to_string(), v))
        .collect();
    let encoded = serde_json::json!({
        "kinds": f.kinds,
        "authors": f.authors,
        "ids": f.ids,
        "tags": tags,
        "since": f.since,
        "until": f.until,
        "limit": f.limit,
    });
    serde_json::to_vec(&encoded)
        .expect("ConcreteFilter's own plain fields always serialize to JSON")
}

impl ConcreteFilter {
    /// Lower to `nostr::Filter` at the resolver/store boundary.
    ///
    /// # Panics
    /// Panics if `authors`/`ids` contain a string that isn't a valid
    /// 32-byte-hex pubkey/event-id, or if a tag key somehow isn't one of
    /// the grammar's valid single-letter tags. Both are construction invariants of
    /// `ConcreteFilter` (its hex strings always originate from
    /// `PublicKey::to_hex`/`EventId::to_hex` round-trips, and its tag keys
    /// are always `IndexedTagName`s, which are pre-validated) — a panic here means
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
                .unwrap_or_else(|e| panic!("IndexedTagName {tag} invariant violated: {e}"));
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

    /// Canonical, stable, collision-resistant hash — the demand/refcount
    /// key (see [`DescriptorHash`]'s doc for why this is BLAKE3, not a
    /// 64-bit hash). Two `ConcreteFilter` values built from the same
    /// logical set of fields but assembled by inserting elements into their
    /// `BTreeSet`/`BTreeMap` fields in a different order hash identically
    /// (`BTreeSet`/`BTreeMap` are already order-normalizing; `blake3::hash`
    /// adds run-to-run/process-to-process stability on top, same as the
    /// FNV implementation this replaced).
    pub fn hash(&self) -> DescriptorHash {
        DescriptorHash(*blake3::hash(&canonical_encoding(self)).as_bytes())
    }
}

/// A resolved demand atom paired with its full identity context (#106):
/// the same [`ConcreteFilter`] requested under two different
/// [`SourceAuthority`]/[`AccessContext`] pairs is TWO distinct atoms —
/// distinct refcount entries, distinct [`DescriptorHash`]es, distinct
/// coverage/attribution identity. This is the anti-alias fix bug-class
/// ledger #18 names: `ConcreteFilter::hash()` alone can never distinguish
/// them (identical bytes hash identically by design), so identity has to
/// widen one level up, here, rather than by mutating `ConcreteFilter`
/// itself (which stays pure selection — untouched by this type).
///
/// Deliberately does NOT carry [`crate::CacheMode`]: cache mode governs the
/// LOCAL row-projection read (#107), never wire/coverage identity, so it is
/// excluded from `hash()`'s input on purpose (atlas's #106/#107 seam
/// ruling) — two `Demand`s differing ONLY in `cache` must still hash
/// (and therefore coalesce) identically.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContextualAtom {
    pub filter: ConcreteFilter,
    pub source: SourceAuthority,
    pub access: AccessContext,
    /// Runtime routing facts projected with this atom. These facts are part
    /// of live atom identity so provenance growth produces an exact
    /// close/open delta, but `nmp-store::coverage_key` deliberately erases
    /// them: route choice must not fragment selection coverage.
    pub routing_evidence: BTreeSet<RoutingEvidence>,
}

impl ContextualAtom {
    /// Canonical, stable, collision-resistant live-atom hash — built from
    /// the filter/context digest plus the canonically ordered routing facts.
    /// An empty evidence set preserves the pre-#11 hash bytes exactly.
    /// Durable coverage deliberately erases routing evidence before calling
    /// this method; see `nmp_store::coverage_key`.
    pub fn hash(&self) -> DescriptorHash {
        let contextual = fold_context(self.filter.hash(), &self.source, self.access);
        if self.routing_evidence.is_empty() {
            return contextual;
        }
        let mut bytes = Vec::new();
        bytes.extend_from_slice(contextual.as_bytes());
        bytes.push(3);
        for evidence in &self.routing_evidence {
            bytes.push(match evidence.origin {
                RoutingEvidenceKind::Hint => 0,
                RoutingEvidenceKind::SourceProvenance => 1,
            });
            let relay = evidence.relay.as_str().as_bytes();
            bytes.extend_from_slice(&(relay.len() as u32).to_be_bytes());
            bytes.extend_from_slice(relay);
        }
        DescriptorHash(*blake3::hash(&bytes).as_bytes())
    }
}

/// Fold `source`/`access` context onto an existing hash, producing a NEW,
/// still framing-unambiguous digest. [`ContextualAtom::hash`] is the
/// primary caller; exposed publicly so a caller with its OWN base hash
/// that isn't a bare `ConcreteFilter::hash()` -- e.g. `nmp-router`'s
/// `Skeleton` hash (authors already erased, for sub-id stability across
/// author churn) or `nmp-store`'s window-erased `CoverageKey` hash -- can
/// derive a context-aware hash without duplicating the tagging scheme or
/// reconstructing a `ContextualAtom` it doesn't otherwise need.
///
/// `source` is a reference (#107): `SourceAuthority` is no longer `Copy`
/// once `Pinned`'s relay set exists, and a caller with only a borrowed
/// atom (the common case) shouldn't need to clone a whole relay set just
/// to hash it.
pub fn fold_context(
    base: DescriptorHash,
    source: &SourceAuthority,
    access: AccessContext,
) -> DescriptorHash {
    let tagged = match source {
        SourceAuthority::AuthorOutboxes => fold_byte(base, 0),
        SourceAuthority::Public => fold_byte(base, 1),
        // #107: two `Pinned` atoms with DIFFERENT relay sets must hash
        // differently (equal filters pinned to R1 vs R2 are genuinely
        // distinct coverage/wire identities) -- fold every relay's own
        // length-prefixed bytes in, not just a fixed discriminant. Members
        // are already canonically ordered (`BTreeSet`), so insertion order
        // never affects the digest.
        SourceAuthority::Pinned(relays) => {
            let mut bytes = Vec::with_capacity(33);
            bytes.extend_from_slice(base.as_bytes());
            bytes.push(2);
            for relay in relays {
                let s = relay.as_str().as_bytes();
                bytes.extend_from_slice(&(s.len() as u32).to_be_bytes());
                bytes.extend_from_slice(s);
            }
            DescriptorHash(*blake3::hash(&bytes).as_bytes())
        }
    };
    fold_byte(
        tagged,
        match access {
            AccessContext::Public => 0,
        },
    )
}

/// Fold one arbitrary tag byte onto an existing hash, producing a NEW,
/// still framing-unambiguous digest (fixed-width, no delimiter needed).
/// [`fold_context`] is built from two calls to this; exposed publicly so a
/// caller needing a differently-shaped tag -- e.g. `nmp-store`'s durable
/// `CoverageKey` schema VERSION tag (Fable's #106 refinement of atlas's C
/// recommendation: a version tag inside the hashed encoding, on top of the
/// context fold, so a future schema change is distinguishable at the hash
/// level too, not just via an outer key prefix) -- can derive one without
/// depending on `blake3` directly itself.
pub fn fold_byte(base: DescriptorHash, tag: u8) -> DescriptorHash {
    let mut bytes = Vec::with_capacity(33);
    bytes.extend_from_slice(base.as_bytes());
    bytes.push(tag);
    DescriptorHash(*blake3::hash(&bytes).as_bytes())
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
                        IndexedTagName::new('d').unwrap(),
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

    /// The load-bearing falsifier for this fix: `DescriptorHash` must be a
    /// 256-bit (32-byte) digest, not the 64-bit FNV-1a value it replaced.
    /// Width is what makes offline collision construction infeasible again
    /// (a 64-bit space is small enough to brute-force/meet-in-the-middle
    /// against an FNV-family hash; 256-bit BLAKE3 is not) -- this pins the
    /// width so a future change can't silently narrow it back down.
    #[test]
    fn descriptor_hash_is_256_bits_wide_not_64() {
        let a = cf(vec!["aa"], vec![]);
        assert_eq!(
            a.hash().as_bytes().len(),
            32,
            "DescriptorHash must be a 32-byte (256-bit) digest"
        );
    }

    /// Two filters that differ in only ONE byte's worth of author-set
    /// content (a single trailing character) must still land in
    /// completely different regions of the digest space -- the avalanche
    /// property a linear hash like FNV-1a does not reliably give (FNV's
    /// output bits are cheap, deterministic functions of the input; small
    /// input deltas can produce small/structured output deltas). This is a
    /// coarse but real regression guard: assert the two digests disagree in
    /// a large majority of their bytes, not just "somewhere".
    #[test]
    fn hash_avalanches_on_a_single_character_change() {
        let a = cf(vec!["aa"], vec![]);
        let b = cf(vec!["ab"], vec![]);
        let da = *a.hash().as_bytes();
        let db = *b.hash().as_bytes();
        let differing_bytes = da.iter().zip(db.iter()).filter(|(x, y)| x != y).count();
        assert!(
            differing_bytes > 20,
            "expected an avalanche (>20/32 bytes differing) for a one-character \
             change, got only {differing_bytes}/32 -- weak diffusion is exactly \
             the property that makes a hash offline-collidable"
        );
    }

    /// #106's anti-alias core: the identical `ConcreteFilter` under two
    /// distinct `SourceAuthority`s must hash to two distinct
    /// `ContextualAtom` identities -- this is precisely the bug-class #18
    /// collapse (same selection, different intended authority, same atom)
    /// the whole `Demand`/`ContextualAtom` widening exists to close.
    #[test]
    fn contextual_atom_hash_distinguishes_identical_filters_under_different_source_authority() {
        let filter = cf(vec!["aa"], vec![]);
        let outbox = ContextualAtom {
            filter: filter.clone(),
            source: crate::descriptor::SourceAuthority::AuthorOutboxes,
            access: crate::descriptor::AccessContext::Public,
            routing_evidence: BTreeSet::new(),
        };
        let public = ContextualAtom {
            filter,
            source: crate::descriptor::SourceAuthority::Public,
            access: crate::descriptor::AccessContext::Public,
            routing_evidence: BTreeSet::new(),
        };
        assert_ne!(
            outbox.hash(),
            public.hash(),
            "same selection under different SourceAuthority must never alias"
        );
    }

    /// #107's headline anti-alias falsifier (Done-when: "Equal filters
    /// pinned to R1 and R2 retain distinct row projections, evidence, EOSE
    /// facts, and teardown"): the IDENTICAL selection pinned to two
    /// DIFFERENT relay sets must hash differently.
    #[test]
    fn contextual_atom_hash_distinguishes_different_pinned_relay_sets() {
        let filter = cf(vec!["aa"], vec![]);
        let r1 = nostr::RelayUrl::parse("wss://r1.example").unwrap();
        let r2 = nostr::RelayUrl::parse("wss://r2.example").unwrap();
        let pinned_r1 = ContextualAtom {
            filter: filter.clone(),
            source: crate::descriptor::SourceAuthority::Pinned(BTreeSet::from([r1])),
            access: crate::descriptor::AccessContext::Public,
            routing_evidence: BTreeSet::new(),
        };
        let pinned_r2 = ContextualAtom {
            filter,
            source: crate::descriptor::SourceAuthority::Pinned(BTreeSet::from([r2])),
            access: crate::descriptor::AccessContext::Public,
            routing_evidence: BTreeSet::new(),
        };
        assert_ne!(
            pinned_r1.hash(),
            pinned_r2.hash(),
            "the same selection pinned to different relay sets must never alias"
        );
    }

    /// Stability companion: the SAME pinned relay set, inserted in a
    /// different order, hashes identically (`BTreeSet` already normalizes
    /// member order; this pins that the fold doesn't accidentally
    /// reintroduce insertion-order sensitivity).
    #[test]
    fn contextual_atom_hash_is_stable_regardless_of_pinned_set_insertion_order() {
        let filter = cf(vec!["aa"], vec![]);
        let r1 = nostr::RelayUrl::parse("wss://r1.example").unwrap();
        let r2 = nostr::RelayUrl::parse("wss://r2.example").unwrap();
        let a = ContextualAtom {
            filter: filter.clone(),
            source: crate::descriptor::SourceAuthority::Pinned(BTreeSet::from([
                r1.clone(),
                r2.clone(),
            ])),
            access: crate::descriptor::AccessContext::Public,
            routing_evidence: BTreeSet::new(),
        };
        let b = ContextualAtom {
            filter,
            source: crate::descriptor::SourceAuthority::Pinned(BTreeSet::from([r2, r1])),
            access: crate::descriptor::AccessContext::Public,
            routing_evidence: BTreeSet::new(),
        };
        assert_eq!(a.hash(), b.hash());
    }

    #[test]
    fn routing_evidence_changes_live_atom_hash() {
        let mut hinted = ContextualAtom {
            filter: cf(vec!["aa"], vec![]),
            source: crate::descriptor::SourceAuthority::Public,
            access: crate::descriptor::AccessContext::Public,
            routing_evidence: BTreeSet::new(),
        };
        let plain = hinted.clone();
        hinted.routing_evidence.insert(RoutingEvidence {
            relay: nostr::RelayUrl::parse("wss://hint.example").unwrap(),
            origin: RoutingEvidenceKind::Hint,
        });
        assert_ne!(plain.hash(), hinted.hash());
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
                    IndexedTagName::new('d').unwrap(),
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
    fn to_nostr_lowers_nip29_h_tag_to_wire_filter() {
        let cf = ConcreteFilter {
            kinds: Some(BTreeSet::from([9u16, 30_315u16])),
            tags: BTreeMap::from([(
                IndexedTagName::new('h').expect("'h' is an ASCII letter"),
                BTreeSet::from(["group-id".to_string()]),
            )]),
            ..ConcreteFilter::default()
        };

        let wire = cf.to_nostr();
        let h_tag = nostr::SingleLetterTag::from_char('h').unwrap();
        assert_eq!(
            wire.generic_tags.get(&h_tag),
            Some(&BTreeSet::from(["group-id".to_string()]))
        );
    }

    /// The full indexed-filter path, not just construction/FFI round-trip
    /// (#64 acceptance evidence / codex-nova review item 2): every `a-z`/
    /// `A-Z` `IndexedTagName` (a) lowers to the EXACT case-preserving
    /// `#<letter>` wire JSON key, and (b) matches an event carrying that
    /// exact tag through the same local `nostr::Filter::match_event` path
    /// the store uses. `x`/`Z` fall out of this loop -- NOT a hand-picked
    /// subset, unlike the whitelist this replaced.
    #[test]
    fn to_nostr_lowers_and_matches_every_ascii_letter_indexed_tag() {
        use nostr::filter::MatchEventOptions;
        use nostr::{EventBuilder, JsonUtil, Keys, Kind, Tag};

        for c in ('a'..='z').chain('A'..='Z') {
            let cf = ConcreteFilter {
                tags: BTreeMap::from([(
                    IndexedTagName::new(c).unwrap(),
                    BTreeSet::from(["v".to_string()]),
                )]),
                ..ConcreteFilter::default()
            };
            let wire = cf.to_nostr();

            // (a) the wire JSON carries the EXACT case-preserving `#<letter>`
            // key -- not folded to a canonical case, not dropped.
            let json = wire.as_json();
            assert!(
                json.contains(&format!("\"#{c}\":[\"v\"]")),
                "expected wire JSON for {c:?} to contain the exact key \"#{c}\", got: {json}"
            );

            // (b) the lowered filter matches an event carrying that exact
            // tag, via the same `nostr::Filter::match_event` path the store
            // uses to serve queries (never a hand-rolled matcher).
            let keys = Keys::generate();
            let event = EventBuilder::new(Kind::Custom(9999), "hi")
                .tag(Tag::parse([c.to_string(), "v".to_string()]).unwrap())
                .sign_with_keys(&keys)
                .expect("test fixture must sign cleanly");
            assert!(
                wire.match_event(&event, MatchEventOptions::new()),
                "filter for #{c} must match an event carrying that exact tag"
            );
        }
    }

    /// Lower/upper-case and distinct-letter indexed tag keys must never
    /// cross-match: a filter built for `e` must not match an event tagged
    /// `E`, and a filter for `x` must not match an event tagged `z` --
    /// exercised through the real `to_nostr`/`match_event` path, not just
    /// `IndexedTagName`'s own `PartialEq`.
    #[test]
    fn to_nostr_indexed_tag_match_is_case_and_letter_exact() {
        use nostr::filter::MatchEventOptions;
        use nostr::{EventBuilder, Keys, Kind, Tag};

        fn filter_for(c: char) -> nostr::Filter {
            ConcreteFilter {
                tags: BTreeMap::from([(
                    IndexedTagName::new(c).unwrap(),
                    BTreeSet::from(["v".to_string()]),
                )]),
                ..ConcreteFilter::default()
            }
            .to_nostr()
        }

        fn event_with_tag(c: char) -> nostr::Event {
            let keys = Keys::generate();
            EventBuilder::new(Kind::Custom(9999), "hi")
                .tag(Tag::parse([c.to_string(), "v".to_string()]).unwrap())
                .sign_with_keys(&keys)
                .expect("test fixture must sign cleanly")
        }

        let opts = MatchEventOptions::new();
        assert!(!filter_for('e').match_event(&event_with_tag('E'), opts));
        assert!(!filter_for('E').match_event(&event_with_tag('e'), opts));
        assert!(!filter_for('x').match_event(&event_with_tag('z'), opts));
        assert!(!filter_for('Z').match_event(&event_with_tag('z'), opts));
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
