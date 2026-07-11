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
    /// The author's kind:10002 READ relay (`routing-and-ownership.md` §2.4 --
    /// the p-tag inbox fan-out consumes THIS set, never `Nip65Write`'s).
    Nip65Read,
    /// Operator-configured app relay (`appRelay`, §2.1) -- every kind, every
    /// author, always, additive; never counted toward the 2-relay-min.
    AppRelay,
    /// Operator-configured fallback relay (§2.1) -- fires per-author only
    /// when that author's own-relay coverage is under the 2-relay-min AND no
    /// `AppRelay` is configured; never counted toward the 2-relay-min itself.
    Fallback,
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
/// SAME trait; M5 ([`LiveDirectory`]) is the first REAL live implementation
/// (the engine feeds it kind:10002 facts at runtime via
/// [`RelayDirectory::ingest_write_relays`]).
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

    /// Operator-configured app relay set (`Lane::AppRelay`, §2.1 of
    /// `routing-and-ownership.md`) -- every kind, every author, always,
    /// additive, never counted toward the 2-relay-min.
    ///
    /// Default: empty. Additive by design: every existing `RelayDirectory`
    /// impl in the workspace keeps compiling unchanged (mirrors
    /// `ingest_write_relays`'s additive pattern above). Only a directory
    /// that is actually configured with an app relay overrides this.
    fn app_relays(&self) -> Vec<RelayUrl> {
        Vec::new()
    }

    /// Operator-configured fallback relay set (`Lane::Fallback`, §2.1).
    /// Applied outside the coverage solve, per-author, only when that
    /// author's own-relay coverage falls under the 2-relay-min AND no
    /// `app_relays` is configured (`app_relays` suppresses fallback
    /// entirely) -- the caller (`nmp-router`) owns that composition; this
    /// accessor only reports the configured set.
    ///
    /// Default: empty.
    fn fallback_relays(&self) -> Vec<RelayUrl> {
        Vec::new()
    }

    /// An author's READ relays (NIP-65 kind:10002 read-marked + unmarked
    /// entries, lane `Nip65Read`) -- distinct from `write_relays`: an
    /// unmarked `r` tag is BOTH read and write, but a `"write"`-marked entry
    /// is excluded here (§2.4). This is what the p-tag inbox fan-out
    /// (`resolve_routes`'s `Default` write policy) consumes for a
    /// recipient, never `write_relays`.
    ///
    /// Default: empty. Additive by design, mirroring `write_relays`'s own
    /// injected-fact shape -- only a directory that actually tracks the
    /// read/write split overrides this.
    fn read_relays(&self, _author: &PubkeyHex) -> Vec<LanedRelay> {
        Vec::new()
    }

    /// True iff this directory has ever recorded a write-relay FACT for
    /// `author` — "known, possibly zero" vs "never resolved". Distinguishes
    /// what `write_relays`'s collapsed `Vec` (empty either way) cannot: an
    /// author whose current kind:10002 declares ZERO write relays is KNOWN
    /// (this returns `true`), not the same as an author never ingested at
    /// all (`false`). `EngineCore::sync_discovery` uses this to stop
    /// discovery for a known-empty author instead of keeping its discovery
    /// subscription open for the rest of the session — without this, an
    /// author who genuinely declares no write relays looks IDENTICAL to one
    /// still awaiting its first kind:10002, and never leaves the "needed"
    /// set.
    ///
    /// Default: `!self.write_relays(author).is_empty()` — preserves
    /// today's collapsed behavior for any directory that hasn't opted into
    /// tracking the distinction (a static/fixture snapshot has no
    /// "not yet resolved" state to begin with: everything it will ever know
    /// is injected upfront). Only [`LiveDirectory`] — the one directory
    /// that actually ingests facts over time — overrides this with the
    /// real per-author ingestion record.
    fn knows_write_relays(&self, author: &PubkeyHex) -> bool {
        !self.write_relays(author).is_empty()
    }

    /// Feed a freshly-ingested NIP-65 write-relay fact for `author` into
    /// this directory, REPLACING whatever it previously held for them
    /// (kind:10002 is a NIP-01 replaceable event -- the caller is expected
    /// to have already resolved the current winner, e.g. via
    /// `EventStore::query`, before calling this: a directory has no event-
    /// ordering/staleness logic of its own). An empty `relays` means the
    /// author's current kind:10002 declares no write relays at all --
    /// still recorded (as "known, zero relays"), not the same as never
    /// having ingested one.
    ///
    /// Default: a no-op. A static/fixture directory (`FixtureDirectory`,
    /// any app-owned snapshot directory) has nothing to update; only a
    /// directory that supports live updates (`LiveDirectory`) overrides
    /// this. Additive by design: every existing `RelayDirectory` impl in
    /// the workspace keeps compiling unchanged.
    fn ingest_write_relays(&mut self, _author: PubkeyHex, _relays: Vec<LanedRelay>) {}

    /// Feed a freshly-ingested NIP-65 READ-relay fact for `author` into this
    /// directory, REPLACING whatever it previously held for them -- the
    /// read-side mirror of `ingest_write_relays`, fed from the SAME
    /// kind:10002 winner in one `ingest_relay_list_winner` pass (§2.4: the
    /// read/write split is parsed off one event, never a second discovery
    /// sub).
    ///
    /// Default: a no-op, for the same reason `ingest_write_relays` defaults
    /// to one -- a static/fixture directory has nothing to update; only
    /// `LiveDirectory` overrides this.
    fn ingest_read_relays(&mut self, _author: PubkeyHex, _relays: Vec<LanedRelay>) {}
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

/// The discovery-kind set (default `{0, 3} ∪ 10000..=19999` -- every
/// NIP-01 REPLACEABLE-range kind, plus kind:0/3 -- owner-affirmed semantics:
/// "discovery = 0/3/1xxxx". An atom whose `kinds` is a (non-empty) subset of
/// this set MAY use the `IndexerDiscovery` lane; a content atom never may
/// (ledger: "indexers are never a content fallback"). Additive with every
/// other role a relay may carry: nothing here excludes a relay that is ALSO
/// one of an author's own kind:10002 write relays from carrying that
/// author's content atoms too (`route::build_candidates` looks up
/// `write_relays`/`indexers` independently and unions the results into one
/// candidate list per author -- see `nmp-router/src/router.rs`'s
/// `additive_relay_role_*` tests).
#[derive(Clone, Debug)]
pub struct DiscoveryKinds(pub BTreeSet<u16>);

impl Default for DiscoveryKinds {
    fn default() -> Self {
        let mut kinds: BTreeSet<u16> = (10_000..=19_999).collect();
        kinds.insert(0);
        kinds.insert(3);
        Self(kinds)
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
    read: BTreeMap<PubkeyHex, Vec<LanedRelay>>,
    app: Vec<RelayUrl>,
    fallback: Vec<RelayUrl>,
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

    /// Add NIP-65 READ relays for `author` (lane `Nip65Read`) -- the fixture
    /// mirror of `with_write`.
    pub fn with_read(
        mut self,
        author: impl Into<PubkeyHex>,
        relays: impl IntoIterator<Item = RelayUrl>,
    ) -> Self {
        let author = author.into();
        self.read.entry(author).or_default().extend(
            relays
                .into_iter()
                .map(|url| LanedRelay::new(url, Lane::Nip65Read)),
        );
        self
    }

    /// Register operator app relays (`Lane::AppRelay`, §2.1).
    pub fn with_app(mut self, relays: impl IntoIterator<Item = RelayUrl>) -> Self {
        self.app.extend(relays);
        self
    }

    /// Register operator fallback relays (`Lane::Fallback`, §2.1).
    pub fn with_fallback(mut self, relays: impl IntoIterator<Item = RelayUrl>) -> Self {
        self.fallback.extend(relays);
        self
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

    fn app_relays(&self) -> Vec<RelayUrl> {
        self.app.clone()
    }

    fn fallback_relays(&self) -> Vec<RelayUrl> {
        self.fallback.clone()
    }

    fn read_relays(&self, author: &PubkeyHex) -> Vec<LanedRelay> {
        self.read.get(author).cloned().unwrap_or_default()
    }

    // `ingest_read_relays` is NOT overridden: a static fixture has nothing
    // to update at runtime, so it relies on the trait's default no-op --
    // mirrors `ingest_write_relays`'s own precedent on this same type.
}

/// A deterministic fixture relay URL (`wss://relay{n}.example.com`), used by
/// tests and the adversarial-mailbox generators above.
pub fn test_relay(n: usize) -> RelayUrl {
    RelayUrl::parse(&format!("wss://relay{n}.example.com")).expect("valid test relay url")
}

/// The engine's own live, updatable [`RelayDirectory`] (M5, "the self-
/// bootstrapping outbox" — replaces `FixtureDirectory`/an app-owned static
/// snapshot as `EngineCore`'s injected directory). Starts knowing ONLY its
/// configured indexer set; write relays begin empty for every author and
/// are filled in over time via [`RelayDirectory::ingest_write_relays`] as
/// the engine ingests each author's kind:10002 — never resolved up front,
/// never touched by an app.
#[derive(Debug, Clone, Default)]
pub struct LiveDirectory {
    write: BTreeMap<PubkeyHex, Vec<LanedRelay>>,
    read: BTreeMap<PubkeyHex, Vec<LanedRelay>>,
    indexers: Vec<RelayUrl>,
    app: Vec<RelayUrl>,
    fallback: Vec<RelayUrl>,
}

impl LiveDirectory {
    /// Start a [`LiveDirectoryBuilder`] (owner-resolved Q5,
    /// `routing-build-plan.md` §7.1: the lane list is still growing --
    /// three lanes land in this milestone alone -- so a builder is the one
    /// edit that absorbs future lanes without churning every construction
    /// site again).
    pub fn builder() -> LiveDirectoryBuilder {
        LiveDirectoryBuilder::default()
    }
}

/// Builder for [`LiveDirectory`]. See [`LiveDirectory::builder`].
#[derive(Default)]
pub struct LiveDirectoryBuilder {
    indexers: Vec<RelayUrl>,
    app: Vec<RelayUrl>,
    fallback: Vec<RelayUrl>,
}

impl LiveDirectoryBuilder {
    /// The operator's fixed discovery-relay set (e.g. the two hardcoded
    /// indexers `nmp-demo` configures) — the ONLY relays a discovery-kind
    /// atom (kind:10002 among them) may ever route to.
    pub fn indexers(mut self, indexers: impl IntoIterator<Item = RelayUrl>) -> Self {
        self.indexers = indexers.into_iter().collect();
        self
    }

    /// The operator's app relay set (`Lane::AppRelay`, §2.1).
    pub fn app_relays(mut self, app: impl IntoIterator<Item = RelayUrl>) -> Self {
        self.app = app.into_iter().collect();
        self
    }

    /// The operator's fallback relay set (`Lane::Fallback`, §2.1).
    pub fn fallback_relays(mut self, fallback: impl IntoIterator<Item = RelayUrl>) -> Self {
        self.fallback = fallback.into_iter().collect();
        self
    }

    pub fn build(self) -> LiveDirectory {
        LiveDirectory {
            write: BTreeMap::new(),
            read: BTreeMap::new(),
            indexers: self.indexers,
            app: self.app,
            fallback: self.fallback,
        }
    }
}

impl RelayDirectory for LiveDirectory {
    fn write_relays(&self, author: &PubkeyHex) -> Vec<LanedRelay> {
        self.write.get(author).cloned().unwrap_or_default()
    }

    fn extra_relays(&self, _author: &PubkeyHex) -> Vec<LanedRelay> {
        Vec::new()
    }

    fn indexers(&self) -> Vec<RelayUrl> {
        self.indexers.clone()
    }

    fn pinned_relays(&self, _atom: &ConcreteFilter) -> Vec<LanedRelay> {
        Vec::new()
    }

    fn app_relays(&self) -> Vec<RelayUrl> {
        self.app.clone()
    }

    fn fallback_relays(&self) -> Vec<RelayUrl> {
        self.fallback.clone()
    }

    fn read_relays(&self, author: &PubkeyHex) -> Vec<LanedRelay> {
        self.read.get(author).cloned().unwrap_or_default()
    }

    fn ingest_write_relays(&mut self, author: PubkeyHex, relays: Vec<LanedRelay>) {
        // Always RECORD the fact, even when `relays` is empty -- per the
        // trait doc's own contract ("still recorded as known, zero relays,
        // not the same as never having ingested one"). The previous
        // `remove`-on-empty implementation violated that contract (an
        // author whose current kind:10002 declares zero write relays looked
        // IDENTICAL to an author never ingested at all): both produced the
        // same `write_relays()` answer (an empty `Vec`), which is the only
        // signal this trait exposes today, so this had no observable effect
        // on routing -- but it is still a real doc/impl mismatch, and a
        // future caller that DOES need to distinguish "known, declares
        // nothing" from "never resolved" (e.g. a `contains_key`-style check)
        // would have silently gotten the wrong answer.
        self.write.insert(author, relays);
    }

    /// The real distinguishing signal `write_relays` alone cannot express:
    /// key PRESENCE in `self.write`, not emptiness of the value. An author
    /// whose kind:10002 declared zero write relays has an entry (inserted
    /// above, even when `relays` is empty); an author never ingested has no
    /// entry at all.
    fn knows_write_relays(&self, author: &PubkeyHex) -> bool {
        self.write.contains_key(author)
    }

    /// The read-side mirror of `ingest_write_relays` -- REPLACES, never
    /// merges, same kind:10002-is-replaceable contract.
    fn ingest_read_relays(&mut self, author: PubkeyHex, relays: Vec<LanedRelay>) {
        self.read.insert(author, relays);
    }
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
    fn live_directory_starts_empty_but_ingests_write_relays_at_runtime() {
        let mut dir = LiveDirectory::builder().indexers([test_relay(9)]).build();
        assert!(dir.write_relays(&pk('a')).is_empty(), "no fact fed in yet");
        assert_eq!(dir.indexers(), vec![test_relay(9)]);

        dir.ingest_write_relays(
            pk('a'),
            vec![LanedRelay::new(test_relay(0), Lane::Nip65Write)],
        );
        let relays = dir.write_relays(&pk('a'));
        assert_eq!(
            relays,
            vec![LanedRelay::new(test_relay(0), Lane::Nip65Write)]
        );
    }

    #[test]
    fn live_directory_ingest_replaces_not_merges() {
        let mut dir = LiveDirectory::builder().build();
        dir.ingest_write_relays(
            pk('a'),
            vec![LanedRelay::new(test_relay(0), Lane::Nip65Write)],
        );
        dir.ingest_write_relays(
            pk('a'),
            vec![LanedRelay::new(test_relay(1), Lane::Nip65Write)],
        );
        assert_eq!(
            dir.write_relays(&pk('a')),
            vec![LanedRelay::new(test_relay(1), Lane::Nip65Write)],
            "a fresh kind:10002 REPLACES the prior write-relay set, never merges"
        );
    }

    /// The load-bearing regression test for the known-empty fix (ledger
    /// #20): `write_relays` alone cannot distinguish "known, declares
    /// zero relays" from "never resolved" (both are an empty `Vec`).
    /// `knows_write_relays` must -- an author whose kind:10002 explicitly
    /// declared zero write relays is KNOWN, so `EngineCore::sync_discovery`
    /// can stop discovering them; an author never ingested at all is not.
    #[test]
    fn live_directory_distinguishes_known_empty_from_never_resolved() {
        let mut dir = LiveDirectory::builder().build();
        assert!(
            !dir.knows_write_relays(&pk('a')),
            "never ingested -- must NOT be considered known"
        );

        dir.ingest_write_relays(pk('a'), Vec::new());
        assert!(
            dir.knows_write_relays(&pk('a')),
            "a kind:10002 declaring zero write relays is still a KNOWN fact"
        );
        assert!(
            dir.write_relays(&pk('a')).is_empty(),
            "write_relays itself still reports empty -- knows_write_relays is the \
             distinguishing signal, not a change to write_relays' own contract"
        );

        // An author who genuinely never got a fact at all stays unknown,
        // even after some OTHER author has been ingested.
        assert!(!dir.knows_write_relays(&pk('z')));
    }

    /// `FixtureDirectory`'s default `knows_write_relays` impl (no override)
    /// collapses back to `!write_relays(..).is_empty()` -- preserving
    /// today's behavior for a static snapshot, which has no "not yet
    /// resolved" state to begin with.
    #[test]
    fn fixture_directory_default_knows_write_relays_matches_non_empty() {
        let dir = FixtureDirectory::new().with_write(pk('a'), [test_relay(0)]);
        assert!(dir.knows_write_relays(&pk('a')));
        assert!(!dir.knows_write_relays(&pk('z')));
    }

    #[test]
    fn default_ingest_write_relays_is_a_no_op_for_fixture_directory() {
        // Additive-trait-method contract: FixtureDirectory never overrides
        // `ingest_write_relays`, so calling it must not panic and must not
        // change what `write_relays` reports.
        let mut dir = FixtureDirectory::new().with_write(pk('a'), [test_relay(0)]);
        dir.ingest_write_relays(
            pk('a'),
            vec![LanedRelay::new(test_relay(5), Lane::Nip65Write)],
        );
        assert_eq!(
            dir.write_relays(&pk('a')),
            vec![LanedRelay::new(test_relay(0), Lane::Nip65Write)]
        );
    }

    /// Unit A's additive-trait contract, mirroring
    /// `default_ingest_write_relays_is_a_no_op_for_fixture_directory` above:
    /// a `FixtureDirectory` that never called `with_app`/`with_fallback`
    /// reports empty for `app_relays`/`fallback_relays`/`read_relays`, and
    /// `ingest_read_relays` (never overridden by `FixtureDirectory`, so it
    /// runs the trait's default no-op) doesn't panic and doesn't change
    /// anything -- every existing `RelayDirectory` impl in the workspace
    /// keeps compiling and behaving unchanged.
    #[test]
    fn defaulted_accessors_are_empty_for_fixture_and_dont_break_existing_impls() {
        let mut dir = FixtureDirectory::new().with_write(pk('a'), [test_relay(0)]);
        assert!(dir.app_relays().is_empty());
        assert!(dir.fallback_relays().is_empty());
        assert!(dir.read_relays(&pk('a')).is_empty());

        dir.ingest_read_relays(
            pk('a'),
            vec![LanedRelay::new(test_relay(5), Lane::Nip65Read)],
        );
        assert!(
            dir.read_relays(&pk('a')).is_empty(),
            "ingest_read_relays is the trait's default no-op for a static fixture"
        );
        assert_eq!(
            dir.write_relays(&pk('a')),
            vec![LanedRelay::new(test_relay(0), Lane::Nip65Write)],
            "unrelated existing facts are untouched"
        );
    }

    #[test]
    fn fixture_directory_with_read_app_fallback_builders() {
        let dir = FixtureDirectory::new()
            .with_read(pk('a'), [test_relay(0)])
            .with_app([test_relay(1)])
            .with_fallback([test_relay(2)]);

        assert_eq!(
            dir.read_relays(&pk('a')),
            vec![LanedRelay::new(test_relay(0), Lane::Nip65Read)]
        );
        assert_eq!(dir.app_relays(), vec![test_relay(1)]);
        assert_eq!(dir.fallback_relays(), vec![test_relay(2)]);
    }

    #[test]
    fn live_directory_builder_wires_app_and_fallback_relays() {
        let dir = LiveDirectory::builder()
            .indexers([test_relay(9)])
            .app_relays([test_relay(1)])
            .fallback_relays([test_relay(2)])
            .build();

        assert_eq!(dir.indexers(), vec![test_relay(9)]);
        assert_eq!(dir.app_relays(), vec![test_relay(1)]);
        assert_eq!(dir.fallback_relays(), vec![test_relay(2)]);
    }

    #[test]
    fn live_directory_ingest_read_relays_replaces_not_merges() {
        let mut dir = LiveDirectory::builder().build();
        dir.ingest_read_relays(
            pk('a'),
            vec![LanedRelay::new(test_relay(0), Lane::Nip65Read)],
        );
        dir.ingest_read_relays(
            pk('a'),
            vec![LanedRelay::new(test_relay(1), Lane::Nip65Read)],
        );
        assert_eq!(
            dir.read_relays(&pk('a')),
            vec![LanedRelay::new(test_relay(1), Lane::Nip65Read)],
            "a fresh kind:10002 REPLACES the prior read-relay set, never merges"
        );
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

    /// Owner-affirmed semantics: discovery = kind:0, kind:3, and the WHOLE
    /// NIP-01 replaceable-event range (10000..=19999), not just the four
    /// kinds NMP happens to read today -- any future replaceable list kind
    /// (kind:10050 DM inbox already included, plus whatever else lands in
    /// that range later) is a discovery kind with zero further code changes.
    #[test]
    fn discovery_kinds_default_covers_the_whole_replaceable_range() {
        let dk = DiscoveryKinds::default();
        assert!(dk.is_discovery(&Some(BTreeSet::from([10_000u16]))));
        assert!(dk.is_discovery(&Some(BTreeSet::from([10_050u16]))));
        assert!(dk.is_discovery(&Some(BTreeSet::from([19_999u16]))));
        assert!(!dk.is_discovery(&Some(BTreeSet::from([9_999u16]))));
        assert!(!dk.is_discovery(&Some(BTreeSet::from([20_000u16]))));
        assert!(!dk.is_discovery(&Some(BTreeSet::from([1u16, 10_002u16]))));
    }
}
