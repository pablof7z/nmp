//! Thread view: a root plus its replies, with reply depth per row.
//!
//! ## What we wanted to write
//!
//! ```text
//! let thread = engine.observe(thread_of(root_id), None)?;
//! for (row, depth) in thread.rows_with_depth() { render(row, depth) }
//! ```
//!
//! ## What we wrote
//!
//! ### The query
//!
//! Two branches: the root by id, and everything whose `#e` names the root.
//! Both are hand-built. `Filter::ids` is a `Binding`, so the root id --
//! an `EventId`, a decoded type -- is stringified to hex to be put in a
//! `Binding::Literal(BTreeSet<String>)`, same as authors.
//!
//! `IndexedTagName::new('e')` is fallible and returns a `Result`, so a
//! constant tag name every Nostr app uses forces an `.expect()` at every call
//! site. There is no `IndexedTagName::E`.
//!
//! The branch that matters is what this query CANNOT say: "everything in the
//! subtree rooted at this event". NIP-10 replies to a reply carry the root in
//! `e`-with-marker-root, so one `#e` branch catches most of a thread and
//! misses any client that only tagged its immediate parent. A correct thread
//! needs recursive expansion. `Selector::Ids` plus a `Derived` binding is
//! exactly the machinery for ONE hop of that -- and a `Derived` binding is a
//! nested struct literal, so N hops means N levels of nesting written out at
//! compile time. There is no recursive or unbounded form. (I did not find a
//! stated depth ceiling in the resolver; the limit here is that the depth has
//! to be a constant in the source, not that some number refuses it.) We do one
//! hop and say so.
//!
//! ### The depth
//!
//! `ThreadPosition::read(&Event)` is the whole of NIP-10 + NIP-22 reading,
//! correctly implemented, in this workspace, tested against every wire shape.
//! Two things stop an app from using it:
//!
//! 1. It is `#[doc(hidden)]` in `nmp` (`crates/nmp/src/lib.rs`, the
//!    `pub use nmp_grammar::{entity_rows, Pointer, RootScope, TagOptions,
//!    TagRows, Tagged, ThreadPosition}` block). An app that imports it is
//!    importing something the facade's own doc says is "the implementor half"
//!    for protocol crates, re-exported "only so an `nmp`-only consumer can
//!    still name the bound on `reply_to`". Reading your own thread's shape is
//!    not naming a bound.
//!
//! 2. It takes `&Event`, and an observation delivers `Row`. `Row::signed_event()`
//!    returns `Option<Event>` -- `None` for a locally accepted row whose
//!    signer has not answered. So THE ONE ROW THE USER JUST WROTE is the one
//!    row whose thread position cannot be read. [`depth_of`] returns `None`
//!    there and [`ThreadView`] renders it at the depth of whatever it replied
//!    to, which the app has to remember separately because the row itself
//!    cannot be asked.
//!
//! `Row` implements `RootScope`, which internally calls `event_for_store()`
//! (sentinel-signed, works for pending rows) and reads exactly this position
//! to build reply tags. So the reading works for pending rows on the WRITE
//! side and is unreachable on the READ side. `Row` has no `thread_position()`.

use std::collections::{BTreeMap, BTreeSet};

use nmp::{
    Binding, Demand, EventId, Filter, IndexedTagName, LiveQuery, LiveQueryError, Row,
    ThreadPosition,
};

use crate::rows::RowTable;

/// The thread's read declaration: the root, plus one hop of replies.
pub fn thread_of(root: EventId) -> Result<LiveQuery, LiveQueryError> {
    let root_hex = BTreeSet::from([root.to_hex()]);
    let the_root = Demand {
        selection: Filter {
            ids: Some(Binding::Literal(root_hex.clone())),
            ..Filter::default()
        },
        ..Demand::default()
    };
    let the_replies = Demand {
        selection: Filter {
            tags: BTreeMap::from([(
                IndexedTagName::new('e').expect("'e' is an ASCII letter"),
                Binding::Literal(root_hex),
            )]),
            ..Filter::default()
        },
        ..Demand::default()
    };
    LiveQuery::union(
        [LiveQuery::single(the_root), LiveQuery::single(the_replies)],
        None,
    )
}

/// The parent this row names, or `None` when it is a root or unreadable.
///
/// Delegates to the grammar's `ThreadPosition` -- which is the right answer and
/// costs a `#[doc(hidden)]` import plus an `Option` that is `None` for every
/// pending row.
#[must_use]
pub fn parent_of(row: &Row) -> Option<EventId> {
    let event = row.signed_event()?;
    let position = ThreadPosition::read(&event);
    position
        .parent
        .and_then(|pointer| pointer.event_id)
        .or_else(|| position.root.and_then(|pointer| pointer.event_id))
}

/// The thread root this row names, or `None`.
#[must_use]
pub fn root_of(row: &Row) -> Option<EventId> {
    let event = row.signed_event()?;
    ThreadPosition::read(&event).root.and_then(|p| p.event_id)
}

/// Reply depth relative to `root`, by walking parent pointers within the
/// rows the app happens to hold.
///
/// Bounded by the table: a reply whose parent is not in the observation gets
/// depth 1 rather than its true depth, because there is nothing to walk to.
/// A real client resolves the missing parents with more queries.
#[must_use]
pub fn depth_of(table: &RowTable, root: EventId, row: &Row) -> Option<usize> {
    if row.id() == root {
        return Some(0);
    }
    let mut depth = 0usize;
    let mut cursor = row.id();
    let mut guard = 0usize;
    loop {
        guard += 1;
        if guard > 64 {
            return None;
        }
        let current = table.get(&cursor)?;
        let Some(parent) = parent_of(current) else {
            // Either a true root, or a pending row we cannot read. The two are
            // indistinguishable here, which is the finding.
            return Some(depth);
        };
        depth += 1;
        if parent == root {
            return Some(depth);
        }
        cursor = parent;
    }
}

/// One thread screen.
pub struct ThreadView {
    root: EventId,
    table: RowTable,
    subscription: nmp::Subscription,
}

impl ThreadView {
    pub fn open(engine: &nmp::Engine, root: EventId) -> Result<Self, ThreadOpenError> {
        let query = thread_of(root).map_err(ThreadOpenError::Query)?;
        let subscription = engine
            .observe(query, None)
            .map_err(ThreadOpenError::Engine)?;
        Ok(Self {
            root,
            table: RowTable::new(),
            subscription,
        })
    }

    pub fn next_within(&mut self, timeout: std::time::Duration) -> Option<nmp::Frame> {
        let frame = self.subscription.recv_timeout(timeout).ok()?;
        self.table.apply(&frame);
        Some(frame)
    }

    /// Rows in reading order: root first, then replies newest-first within
    /// each depth. Every part of this ordering is the app's invention -- the
    /// unbounded delivery mode states no order at all.
    #[must_use]
    pub fn rendered(&self) -> Vec<(usize, &Row)> {
        let mut out: Vec<(usize, &Row)> = self
            .table
            .rows()
            .map(|row| (depth_of(&self.table, self.root, row).unwrap_or(1), row))
            .collect();
        out.sort_by(|(left_depth, left), (right_depth, right)| {
            left_depth
                .cmp(right_depth)
                .then_with(|| right.created_at().cmp(&left.created_at()))
                .then_with(|| left.id().cmp(&right.id()))
        });
        out
    }

    #[must_use]
    pub fn table(&self) -> &RowTable {
        &self.table
    }

    #[must_use]
    pub fn root(&self) -> EventId {
        self.root
    }
}

#[derive(Debug)]
pub enum ThreadOpenError {
    Query(LiveQueryError),
    Engine(nmp::EngineError),
}

impl std::fmt::Display for ThreadOpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Query(error) => write!(f, "thread query: {error}"),
            Self::Engine(error) => write!(f, "thread observation: {error}"),
        }
    }
}

impl std::error::Error for ThreadOpenError {}
