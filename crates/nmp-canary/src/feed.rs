//! The public follows feed: `authors = follows(current) - mutes(current)`,
//! kinds 1 and 6, paged, live.
//!
//! ## What we wanted to write
//!
//! ```text
//! let feed = engine.observe(
//!     following(Kind::TextNote, Kind::Repost).minus(muted()),
//!     Window::newest(50),
//! )?;
//! ```
//!
//! ## What we wrote
//!
//! [`follows_feed`], below: 40 lines of nested struct literal for one
//! sentence. Look at what is actually being said and what it costs to say it.
//!
//! - "my follows" is `Derived { inner: Demand { selection: Filter { kinds:
//!   [3], authors: Reactive(ActivePubkey) } }, project: Tag("p") }`. That is
//!   the whole definition of NIP-02's follow list -- kind, projection and all
//!   -- restated here in an app. `nmp-nip02` OWNS this and exports
//!   `current_account_demand()` for the kind:3 half... but it returns the
//!   `Demand`, and what a feed needs is the `Binding` that PROJECTS it. The
//!   `project: Selector::Tag("p")` step is the part with the NIP-02 knowledge
//!   in it, and it is the part the capability crate does not supply. We call
//!   `nmp_nip02::current_account_demand()` and then hand-write the projection
//!   beside it, which is the exact seam where an app can get NIP-02 wrong
//!   while looking like it used the library.
//!
//! - "my mutes" is the same shape over kind:10000 -- and there is NO mute
//!   capability crate at all. Verified: `git grep -n "10000\|MuteList" --
//!   crates` returns `nmp-resolver-testkit`'s `kind10000_mutes` FIXTURE, a
//!   `nmp-resolver` test, and two comments in the grammar explaining that
//!   `SetAlgebra::Diff` exists so "follows minus mutes" is declarable. The
//!   engine was built for this exact sentence and the vocabulary half of it
//!   ships only as test scaffolding. So [`MUTE_LIST_KIND`] and its `p`
//!   projection are declared in this app.
//!
//! - "minus" is `Binding::SetOp(Box::new(SetOp { op: SetAlgebra::Diff,
//!   operands: vec![follows, mutes] }))`. `SetOp` is NOT re-exported by `nmp`
//!   -- `nmp`'s re-export list names `SetAlgebra` and `SetOp` the enum... it
//!   names both. It does. But `Binding::SetOp` takes `Box<SetOp>` and `SetOp`
//!   is a struct with public fields, so the app writes `Box::new(SetOp { .. })`
//!   with no constructor and no combinator. `Binding` has no `.minus()`.
//!
//! - "kinds 1 and 6" is a `BTreeSet<u16>` of bare integers. `nmp` re-exports
//!   `TEXT_NOTE_KIND` (1) from the tagging door and `nmp-nip18` exports
//!   `REPOST_KIND` (6), but `Filter::kinds` is `Option<BTreeSet<u16>>` while
//!   the write side speaks `Kind`. The read grammar and the write grammar
//!   disagree about what a kind is.
//!
//! ## Paging
//!
//! `Window::Expandable { initial, max }` plus `Subscription::request_rows`.
//! This part works and is genuinely good: the window delivers the complete
//! current row set already sorted newest-first, so [`FollowsFeed`] does not
//! need `RowTable` at all. It is the only surface in this app that does not.
//!
//! The catch is stated in `Engine::observe`'s own doc: a window and a
//! `Filter::limit` are mutually exclusive (`WindowSelectionHasLimit`). A feed
//! that wants "the newest 50 from each of my follows' relays, presented as one
//! list" cannot say both. We take the window and let the relays decide their
//! own bound.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;

use nmp::{
    Binding, Demand, Derived, Engine, EngineError, Filter, Frame, LiveQuery, PublicKey, Row,
    Selector, SetAlgebra, SetOp, Subscription, Window,
};

/// NIP-51 kind:10000. Declared here because no crate in this workspace owns
/// it -- see the module doc for the search that establishes that.
pub const MUTE_LIST_KIND: u16 = 10_000;
/// NIP-01 kind:1. `nmp::TEXT_NOTE_KIND` is a `Kind`; `Filter::kinds` wants a
/// `u16`.
pub const TEXT_NOTE: u16 = 1;
/// NIP-18 kind:6. `nmp_nip18::REPOST_KIND` is already a `u16`, so this one is
/// borrowed rather than restated.
pub const REPOST: u16 = nmp_nip18::REPOST_KIND;

/// "The people I follow" as a projected value set.
///
/// The kind:3 half comes from `nmp-nip02`. The projection does not, so the
/// `"p"` string literal below is this app asserting a NIP-02 fact.
#[must_use]
pub fn follows_binding() -> Binding {
    Binding::Derived(Box::new(Derived {
        inner: nmp_nip02::current_account_demand(),
        project: Selector::Tag("p".to_string()),
    }))
}

/// "The people I mute" as a projected value set. Entirely app-owned.
#[must_use]
pub fn mutes_binding() -> Binding {
    Binding::Derived(Box::new(Derived {
        inner: Demand {
            selection: Filter {
                kinds: Some(BTreeSet::from([MUTE_LIST_KIND])),
                authors: Some(Binding::Reactive(nmp::IdentityField::ActivePubkey)),
                ..Filter::default()
            },
            ..Demand::default()
        },
        project: Selector::Tag("p".to_string()),
    }))
}

/// `follows - mutes`.
#[must_use]
pub fn follows_minus_mutes() -> Binding {
    Binding::SetOp(Box::new(SetOp {
        op: SetAlgebra::Diff,
        operands: vec![follows_binding(), mutes_binding()],
    }))
}

/// The feed's read declaration.
#[must_use]
pub fn follows_feed() -> LiveQuery {
    LiveQuery::single(follows_feed_demand())
}

/// The same thing as a bare `Demand`, because `profiles::profiles_of_query`
/// needs the demand and not the query -- a `Derived` binding nests a `Demand`,
/// while `Engine::observe` takes a `LiveQuery`, so an app that wants both an
/// observation and a projection of it keeps the `Demand` around and builds the
/// `LiveQuery` twice.
#[must_use]
pub fn follows_feed_demand() -> Demand {
    Demand {
        selection: Filter {
            kinds: Some(BTreeSet::from([TEXT_NOTE, REPOST])),
            authors: Some(follows_minus_mutes()),
            ..Filter::default()
        },
        ..Demand::default()
    }
}

/// One page-able live feed screen.
pub struct FollowsFeed {
    subscription: Subscription,
    /// The window's own rows, kept verbatim from the last frame. No fold, no
    /// re-sort: this is what the windowed mode hands over, and it is the one
    /// surface here that gets to be this small.
    rows: Vec<Row>,
    load: Option<nmp::WindowLoad>,
    /// Per-branch acquisition evidence from the last frame, zipped back onto
    /// the branches by index. See [`FollowsFeed::empty_state`].
    evidence: Vec<nmp::AcquisitionEvidence>,
    query: LiveQuery,
}

impl FollowsFeed {
    /// Open the feed with a starting page size and a hard ceiling.
    pub fn open(engine: &Engine, page: usize, max: usize) -> Result<Self, EngineError> {
        let initial = NonZeroUsize::new(page.max(1)).expect("max(1) is nonzero");
        let max = NonZeroUsize::new(max.max(page).max(1)).expect("max(1) is nonzero");
        let query = follows_feed();
        let subscription =
            engine.observe(query.clone(), Some(Window::Expandable { initial, max }))?;
        Ok(Self {
            subscription,
            rows: Vec::new(),
            load: None,
            evidence: Vec::new(),
            query,
        })
    }

    /// Take the next delivery, blocking.
    ///
    /// `Subscription::recv()` blocks the calling thread. A real UI runs this on
    /// its own thread and posts to the main loop, or uses the `AsyncSubscription`
    /// twin -- which is `#[doc(hidden)]` and reached through the equally
    /// `#[doc(hidden)]` `Engine::observe_async`. This app uses the documented
    /// blocking door here and the hidden async one in `room`, because
    /// `nmp-nip29`'s room observation is async-only. See `findings::ASYNC_SPLIT`.
    pub fn recv(&mut self) -> Option<Frame> {
        let frame = self.subscription.recv().ok()?;
        self.absorb(&frame);
        Some(frame)
    }

    /// Deadline-bounded twin, for the exerciser.
    ///
    /// Named `recv`/`next_within` after a coin toss: the workspace's four
    /// delivery types spell the same act four ways -- `Subscription::recv`,
    /// `AsyncSubscription::next`, `GroupObservation::next` and `next_within`,
    /// `FollowObservation::recv` and `recv_timeout`, `GroupObservation::latest`.
    /// An app wrapping all of them picks one and translates.
    pub fn next_within(&mut self, timeout: std::time::Duration) -> Option<Frame> {
        let frame = self.subscription.recv_timeout(timeout).ok()?;
        self.absorb(&frame);
        Some(frame)
    }

    fn absorb(&mut self, frame: &Frame) {
        if let Some(contents) = &frame.window {
            self.rows = contents.rows.clone();
            self.load = Some(contents.load);
        }
        self.evidence = frame.evidence.clone();
    }

    /// Grow the window. Declarative: no cursor, no continuation token.
    pub fn load_more(&self, at_least: usize) -> Result<(), nmp::RequestRowsError> {
        self.subscription.request_rows(at_least)
    }

    #[must_use]
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    #[must_use]
    pub fn load(&self) -> Option<nmp::WindowLoad> {
        self.load
    }

    #[must_use]
    pub fn authors(&self) -> Vec<PublicKey> {
        let mut authors: Vec<PublicKey> = self.rows.iter().map(Row::pubkey).collect();
        authors.sort_unstable();
        authors.dedup();
        authors
    }

    /// What to put on the screen when there are no rows.
    ///
    /// This is the surface the suspicion "evidence sits beside the rows rather
    /// than on the absence" was about, and the honest answer is more nuanced
    /// than the suspicion. `ShortfallFact::NoResolvedDemand` is EXACTLY "the
    /// derived set is currently empty", i.e. "you follow nobody yet" -- an
    /// explanation attached to the emptiness. It is there and it is good.
    ///
    /// What is missing is WHICH branch and WHICH derived set. `ShortfallFact`
    /// names no binding, and `NoPlannedSource`/`LocalLimit` carry a
    /// `ConcreteFilter` -- a type `nmp` does NOT re-export, so this app can
    /// pattern-match the variant and cannot read the atom inside it. A feed
    /// whose follows list is empty and a feed whose mute list is unreachable
    /// produce shortfall facts an `nmp`-only app cannot tell apart.
    #[must_use]
    pub fn empty_state(&self) -> EmptyState {
        if !self.rows.is_empty() {
            return EmptyState::HasRows;
        }
        let branches = self.query.branches();
        for (index, evidence) in self.evidence.iter().enumerate() {
            let _branch: Option<&Demand> = branches.get(index);
            // One fact decides the whole message, because there is nothing to
            // rank them by: `ShortfallFact` names no binding and no branch, so
            // a query with two shortfalls has no way to say which one the user
            // should be told about. First wins.
            if let Some(fact) = evidence.shortfall.first() {
                return match fact {
                    nmp::ShortfallFact::NoResolvedDemand => EmptyState::NothingDemanded,
                    nmp::ShortfallFact::NoPlannedSource { .. } => EmptyState::NoSourceForSomeAtom,
                    nmp::ShortfallFact::LocalLimit { .. } => EmptyState::RelayCeilingHit,
                };
            }
            if evidence
                .sources
                .iter()
                .any(|source| matches!(source.status, nmp::SourceStatus::Error))
            {
                return EmptyState::SourceFailing;
            }
            if evidence.sources.is_empty() {
                return EmptyState::NoSourcesYet;
            }
        }
        EmptyState::Acquiring
    }

    /// Per-relay acquisition status, for the feed header's little dot.
    ///
    /// Note the shape: `Frame.evidence` is one entry PER BRANCH, and each entry
    /// holds one `SourceEvidence` per (relay, identity) session. An app that
    /// wants "is this relay healthy" -- one row per relay in a settings screen
    /// -- has to fold across branches itself, and two branches disagreeing
    /// about one relay is representable and unrendered by this fold.
    #[must_use]
    pub fn per_relay(&self) -> BTreeMap<String, nmp::SourceStatus> {
        let mut out = BTreeMap::new();
        for evidence in &self.evidence {
            for source in &evidence.sources {
                out.insert(source.relay.to_string(), source.status);
            }
        }
        out
    }
}

/// Why the feed is empty. Assembled by the app from shortfall facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmptyState {
    HasRows,
    /// `ShortfallFact::NoResolvedDemand` -- the derived author set resolved to
    /// nothing. In THIS query that means "you follow nobody", but the fact does
    /// not say so; a query with two derived bindings gets the same word.
    NothingDemanded,
    NoSourceForSomeAtom,
    RelayCeilingHit,
    SourceFailing,
    NoSourcesYet,
    Acquiring,
}
