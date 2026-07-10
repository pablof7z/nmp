//! Lane facts + the injected mailbox/relay-fact surface (M2 plan §2.1).
//!
//! Everything here is a **fixture lookup, no network** — `nmp-router`
//! depends only on `nmp-grammar`/`nostr`; live NIP-65 fetching / relay
//! probing arrives in M3 behind the SAME [`RelayDirectory`] trait.

use std::collections::{BTreeMap, BTreeSet};

pub use nostr::RelayUrl;

use nmp_grammar::ConcreteFilter;

/// Matches `ConcreteFilter.authors`'s element type.
pub type PubkeyHex = String;

/// The lane every relay-bearing fact and every route carries (VISION P4,
/// ledger #3). CLOSED vocabulary — extend the enum, never admit a
/// free-form string.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Lane {
    /// The author's kind:10002 WRITE relay (the outbox default).
    Nip65Write,
    /// A relay hint from a tag / nevent / nprofile.
    Hint,
    /// Where we've previously seen this author's events.
    Provenance,
    /// Operator policy (role-tagged config, not a route override).
    UserConfigured,
    /// Operator indexer set — DISCOVERY KINDS ONLY, never a content fallback.
    IndexerDiscovery,
    /// NIP-29 host relay for a non-author group atom (pinned).
    GroupHost,
    /// kind:10050 DM inbox (pinned; full private-route provenance is M3).
    DmInbox,
}

/// A relay tagged with the lane that supplied it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LanedRelay {
    pub url: RelayUrl,
    pub lane: Lane,
}

impl LanedRelay {
    pub fn new(url: RelayUrl, lane: Lane) -> Self {
        Self { url, lane }
    }
}

/// The injected mailbox/relay-fact surface. In M2 every method is a fixture
/// lookup (no network). M3 backs this with live NIP-65 / probing behind the
/// SAME trait.
pub trait RelayDirectory {
    /// An author's write relays (NIP-65 kind:10002 write entries), lane-tagged.
    fn write_relays(&self, author: &PubkeyHex) -> Vec<LanedRelay>;
    /// Hint / provenance / user-configured extras for an author (may be empty).
    fn extra_relays(&self, author: &PubkeyHex) -> Vec<LanedRelay>;
    /// Operator indexer set. Eligible ONLY for discovery-kind atoms.
    fn indexers(&self) -> Vec<RelayUrl>;
    /// Pinned relays for a NON-author atom (NIP-29 group host, DM inbox, …).
    /// Empty => unroutable.
    fn pinned_relays(&self, atom: &ConcreteFilter) -> Vec<LanedRelay>;
}

/// Configurable relay limits — the kill measurement (test 16) asserts the
/// compiled plan stays within these. Defaults reflect v1 evidence (relays
/// accept large author arrays but cap concurrent subscriptions).
#[derive(Clone, Copy, Debug)]
pub struct RelayLimits {
    pub max_subs_per_relay: usize,
    pub max_filter_authors: usize,
    pub max_filter_terms: usize,
}

impl Default for RelayLimits {
    fn default() -> Self {
        Self {
            max_subs_per_relay: 20,
            max_filter_authors: 1_000,
            max_filter_terms: 1_000,
        }
    }
}

/// The discovery-kind set (default `{0, 3, 10002, 10050}`). An atom whose
/// `kinds` is a (non-empty) subset of this set MAY use the
/// `IndexerDiscovery` lane; a content atom never may (ledger: "indexers are
/// never a content fallback").
#[derive(Clone, Debug)]
pub struct DiscoveryKinds(pub BTreeSet<u16>);

impl Default for DiscoveryKinds {
    fn default() -> Self {
        Self(BTreeSet::from([0, 3, 10002, 10050]))
    }
}

impl DiscoveryKinds {
    /// True iff `kinds` is non-empty and every member is a discovery kind.
    pub fn is_discovery(&self, kinds: &Option<BTreeSet<u16>>) -> bool {
        match kinds {
            Some(ks) if !ks.is_empty() => ks.iter().all(|k| self.0.contains(k)),
            _ => false,
        }
    }
}

/// An in-memory fixture implementation of [`RelayDirectory`], with ergonomic
/// builders and a few adversarial-mailbox generators used by the solver
/// tests (M2 plan §3). Pinned relays are keyed by the exact `ConcreteFilter`
/// atom (non-author atoms carry no `authors`, so equality on the atom's
/// discriminating fields — `kinds`/`tags` — is the natural fixture key).
#[derive(Default, Clone)]
pub struct FixtureDirectory {
    write: BTreeMap<PubkeyHex, Vec<LanedRelay>>,
    extra: BTreeMap<PubkeyHex, Vec<LanedRelay>>,
    indexers: Vec<RelayUrl>,
    pinned: BTreeMap<ConcreteFilter, Vec<LanedRelay>>,
}

impl FixtureDirectory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add NIP-65 write relays for `author` (lane `Nip65Write`).
    pub fn with_write(
        mut self,
        author: impl Into<PubkeyHex>,
        relays: impl IntoIterator<Item = RelayUrl>,
    ) -> Self {
        let author = author.into();
        self.write.entry(author).or_default().extend(
            relays
                .into_iter()
                .map(|url| LanedRelay::new(url, Lane::Nip65Write)),
        );
        self
    }

    /// Add extra (non-write) relays for `author` under an explicit lane
    /// (`Hint`/`Provenance`/`UserConfigured`).
    pub fn with_extra(
        mut self,
        author: impl Into<PubkeyHex>,
        lane: Lane,
        relays: impl IntoIterator<Item = RelayUrl>,
    ) -> Self {
        let author = author.into();
        self.extra
            .entry(author)
            .or_default()
            .extend(relays.into_iter().map(|url| LanedRelay::new(url, lane)));
        self
    }

    /// Register an operator indexer relay.
    pub fn with_indexer(mut self, relay: RelayUrl) -> Self {
        self.indexers.push(relay);
        self
    }

    /// Register pinned relays for a non-author atom (NIP-29 group host, DM
    /// inbox, …), keyed by the atom's exact `ConcreteFilter` value.
    pub fn with_pinned(mut self, atom: ConcreteFilter, relays: Vec<LanedRelay>) -> Self {
        self.pinned.entry(atom).or_default().extend(relays);
        self
    }

    /// Convenience: register a single `GroupHost`-lane pinned relay.
    pub fn with_group_host(self, atom: ConcreteFilter, relay: RelayUrl) -> Self {
        self.with_pinned(atom, vec![LanedRelay::new(relay, Lane::GroupHost)])
    }

    /// Convenience: register a single `DmInbox`-lane pinned relay.
    pub fn with_dm_inbox(self, atom: ConcreteFilter, relay: RelayUrl) -> Self {
        self.with_pinned(atom, vec![LanedRelay::new(relay, Lane::DmInbox)])
    }

    // ---- adversarial-mailbox generators (M2 plan §3 solver tests) -------

    /// `authors.len()` authors, each with its own DISJOINT pair of write
    /// relays (no overlap across authors) — the "disjoint mailboxes"
    /// adversarial case.
    pub fn disjoint_mailboxes(authors: &[PubkeyHex]) -> Self {
        let mut dir = Self::new();
        for (i, author) in authors.iter().enumerate() {
            let r1 = test_relay(i * 2);
            let r2 = test_relay(i * 2 + 1);
            dir = dir.with_write(author.clone(), [r1, r2]);
        }
        dir
    }

    /// Every author in `authors` shares the exact same `pool` of write
    /// relays — the "heavy overlap" adversarial case.
    pub fn shared_pool_mailboxes(authors: &[PubkeyHex], pool: &[RelayUrl]) -> Self {
        let mut dir = Self::new();
        for author in authors {
            dir = dir.with_write(author.clone(), pool.iter().cloned());
        }
        dir
    }

    /// A single author with `n` distinct write relays — the "one prolific
    /// author" adversarial case.
    pub fn prolific_author(author: impl Into<PubkeyHex>, n: usize) -> Self {
        Self::new().with_write(author, (0..n).map(test_relay))
    }
}

impl RelayDirectory for FixtureDirectory {
    fn write_relays(&self, author: &PubkeyHex) -> Vec<LanedRelay> {
        self.write.get(author).cloned().unwrap_or_default()
    }

    fn extra_relays(&self, author: &PubkeyHex) -> Vec<LanedRelay> {
        self.extra.get(author).cloned().unwrap_or_default()
    }

    fn indexers(&self) -> Vec<RelayUrl> {
        self.indexers.clone()
    }

    fn pinned_relays(&self, atom: &ConcreteFilter) -> Vec<LanedRelay> {
        self.pinned.get(atom).cloned().unwrap_or_default()
    }
}

/// A deterministic fixture relay URL (`wss://relay{n}.example.com`), used by
/// tests and the adversarial-mailbox generators above.
pub fn test_relay(n: usize) -> RelayUrl {
    RelayUrl::parse(&format!("wss://relay{n}.example.com")).expect("valid test relay url")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pk(c: char) -> PubkeyHex {
        c.to_string().repeat(64)
    }

    #[test]
    fn write_relays_lane_tagged_nip65_write() {
        let dir = FixtureDirectory::new().with_write(pk('a'), [test_relay(0), test_relay(1)]);
        let relays = dir.write_relays(&pk('a'));
        assert_eq!(relays.len(), 2);
        assert!(relays.iter().all(|r| r.lane == Lane::Nip65Write));
    }

    #[test]
    fn unknown_author_has_no_relays() {
        let dir = FixtureDirectory::new();
        assert!(dir.write_relays(&pk('z')).is_empty());
        assert!(dir.extra_relays(&pk('z')).is_empty());
    }

    #[test]
    fn disjoint_mailboxes_generator_gives_each_author_two_unique_relays() {
        let authors = vec![pk('a'), pk('b'), pk('c')];
        let dir = FixtureDirectory::disjoint_mailboxes(&authors);
        let mut all_urls = BTreeSet::new();
        for a in &authors {
            let relays = dir.write_relays(a);
            assert_eq!(relays.len(), 2);
            for r in relays {
                assert!(all_urls.insert(r.url), "expected no overlap across authors");
            }
        }
    }

    #[test]
    fn discovery_kinds_default_and_classification() {
        let dk = DiscoveryKinds::default();
        assert!(dk.is_discovery(&Some(BTreeSet::from([0u16]))));
        assert!(dk.is_discovery(&Some(BTreeSet::from([3u16, 10002u16]))));
        assert!(!dk.is_discovery(&Some(BTreeSet::from([1u16]))));
        assert!(!dk.is_discovery(&Some(BTreeSet::from([1u16, 3u16]))));
        assert!(!dk.is_discovery(&None));
    }
}
