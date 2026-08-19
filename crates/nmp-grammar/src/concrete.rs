//! [`ConcreteFilter`] — a fully-resolved filter (no bindings), the unit of
//! the demand set and the refcount/dedup key, plus [`DescriptorHash`], its
//! canonical hash.

use std::collections::{BTreeMap, BTreeSet};

use crate::descriptor::ReadRouting;
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
/// A canonical, order-normalised byte encoding of `f`, used to build
/// durable keys. Deliberately NOT digested: a hash of this answers only
/// "byte-identical?", the least useful question about a filter, and
/// destroys what a containment or residual question needs.
pub fn canonical_encoding(f: &ConcreteFilter) -> Vec<u8> {
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
/// [`ReadRouting`]/authenticated-identity pairs is TWO distinct atoms —
/// distinct refcount entries, distinct [`DescriptorHash`]es, distinct
/// coverage/attribution identity. This is the anti-alias fix guarantee
/// #18 names: `ConcreteFilter::hash()` alone can never distinguish
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
    pub routing: ReadRouting,
    /// The identity these reads authenticate as, carried verbatim from the
    /// demand and passed straight into the session key by `nmp-router`.
    /// Nothing resolves it and no current account is consulted — this crate
    /// has no access to one, and an atom must not change identity when the
    /// account changes.
    ///
    /// It is part of atom identity because two demands naming different
    /// identities are genuinely different acquisitions: what a relay serves
    /// depends on who asked. It is the same value that keys durable coverage
    /// (`nmp_store::coverage_key`).
    pub authenticate_as: Option<nostr::PublicKey>,
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
        let contextual = fold_context(self.filter.hash(), &self.routing, self.authenticate_as);
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

/// Fold `source`/identity context onto an existing hash, producing a NEW,
/// still framing-unambiguous digest. [`ContextualAtom::hash`] is the
/// primary caller; exposed publicly so a caller with its OWN base hash
/// that isn't a bare `ConcreteFilter::hash()` -- e.g. `nmp-router`'s
/// `Skeleton` hash (authors already erased, for sub-id stability across
/// author churn) or `nmp-store`'s window-erased `CoverageKey` hash -- can
/// derive a context-aware hash without duplicating the tagging scheme or
/// reconstructing a `ContextualAtom` it doesn't otherwise need.
///
/// `routing` is a reference: [`ReadRouting`] is not `Copy` once `Explicit`'s
/// relay set exists, and a caller with only a borrowed atom (the common
/// case) shouldn't need to clone a whole relay set just to hash it.
pub fn fold_context(
    base: DescriptorHash,
    routing: &ReadRouting,
    authenticate_as: Option<nostr::PublicKey>,
) -> DescriptorHash {
    let tagged = match routing {
        ReadRouting::Auto => fold_byte(base, 0),
        // Two `Explicit` atoms with DIFFERENT relay sets must hash
        // differently (equal filters routed to R1 vs R2 are genuinely
        // distinct coverage/wire identities) -- fold every relay's own
        // length-prefixed bytes in, not just a fixed discriminant.
        //
        // The `Vec` is folded in ITS OWN order, which is what makes this
        // digest agree with the derived `Ord`/`Hash` on `ReadRouting` for
        // every possible value, normalized or not: both read the same
        // sequence. `Demand::new` separately normalizes on the way in so
        // one routing intent has one representation, but the two mechanisms
        // are independent — the digest cannot disagree with identity even
        // for a `Vec` that never passed through `Demand::new`.
        ReadRouting::Explicit(relays) => {
            let mut bytes = Vec::with_capacity(33);
            bytes.extend_from_slice(base.as_bytes());
            bytes.push(1);
            for relay in relays {
                let s = relay.as_str().as_bytes();
                bytes.extend_from_slice(&(s.len() as u32).to_be_bytes());
                bytes.extend_from_slice(s);
            }
            DescriptorHash(*blake3::hash(&bytes).as_bytes())
        }
    };
    // Absent and present MUST fold to distinct, stable bytes, and the two
    // arms must stay framing-unambiguous against each other: absent folds a
    // bare `0` tag byte, present folds a `1` tag byte followed by the key's
    // fixed 32 bytes. A present key can therefore never be mistaken for an
    // absent one whose base digest happened to end in the key's bytes — the
    // tag byte sits between them and the width is fixed.
    match authenticate_as {
        None => fold_byte(tagged, 0),
        Some(public_key) => {
            let mut bytes = Vec::with_capacity(65);
            bytes.extend_from_slice(tagged.as_bytes());
            bytes.push(1);
            bytes.extend_from_slice(&public_key.to_bytes());
            DescriptorHash(*blake3::hash(&bytes).as_bytes())
        }
    }
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

