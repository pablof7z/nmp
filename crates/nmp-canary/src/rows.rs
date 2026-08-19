//! The parallel row table every single surface in this app had to build.
//!
//! ## What we wanted to write
//!
//! ```text
//! let feed = engine.observe(query, Window::newest(50))?;
//! for row in feed.rows() { render(row) }   // ordered, complete, current
//! ```
//!
//! ## What we wrote
//!
//! This file. Twice over, because the two delivery modes hand back different
//! shapes and neither one is "the current list":
//!
//! - An UNBOUNDED observation delivers `Frame { deltas, window: None, .. }`.
//!   The app is told what changed and must own the set. `Frame` never carries
//!   the current rows, so there is no way to render without this table.
//! - A WINDOWED observation delivers `Frame { window: Some(WindowContents {
//!   rows, .. }), .. }` -- the complete current set, already sorted -- but it
//!   is bounded, growable only by `request_rows`, and `Filter::limit` and a
//!   window are mutually exclusive (`EngineError::WindowSelectionHasLimit`).
//!
//! So a screen that wants "the newest 50, growing as you scroll" uses the
//! window and this table is dead weight; a screen that wants "everything that
//! matches, live" uses deltas and this table is mandatory. Neither of the
//! seven surfaces below could pick one mode and stay in it.
//!
//! ## Ordering
//!
//! `RowDelta` carries no order. `Frame.deltas` is a transition, not a
//! sequence, and the `Added` rows inside one frame are in no stated order
//! either. Every unbounded consumer therefore re-sorts its whole table on
//! every delivery -- `sort_unstable_by` over N rows per frame, forever. The
//! windowed mode DOES define an order ("canonical newest-first
//! (`created_at DESC, event_id ASC`)") and the unbounded mode does not, which
//! is why `Ordering::CANONICAL` below is this app restating a sort the engine
//! already knows how to do.

use std::collections::BTreeMap;

use nmp::{EventId, Frame, PublicKey, RelayUrl, Row, RowDelta};

/// The app-owned accumulation of one unbounded observation.
///
/// Keyed by event id because `RowDelta::Removed` carries only an id, and
/// `RowDelta::SourcesGrew` carries an id plus the new source set -- a patch we
/// must apply against a row we already hold, since `SourcesGrew` deliberately
/// does not carry the body.
#[derive(Debug, Default)]
pub struct RowTable {
    by_id: BTreeMap<EventId, Row>,
    /// Re-derived from `by_id` after every delivery. Held separately because
    /// every render pass wants a slice and `BTreeMap` iteration order is by
    /// event id, which is not an order any human wants to read.
    ordered: Vec<EventId>,
    /// Set when the last `apply` changed anything, so a UI can skip a repaint.
    /// Nothing on `Frame` says whether a delivery was a no-op.
    dirty: bool,
}

impl RowTable {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one delivered frame into the table.
    ///
    /// This exact function -- match four variants, insert/remove, then re-sort
    /// -- appears in `nmp-nip02`'s follow observation (`observe.rs`'s
    /// `Accumulator`) and in `nmp-nip29`'s record observation as well. Three
    /// independent implementations of the same fold is the measurement: the
    /// engine knows how to hold a row set (the windowed mode does exactly
    /// this) and does not offer it on the unbounded one.
    pub fn apply(&mut self, frame: &Frame) {
        let before = self.by_id.len();
        let mut touched = false;
        for delta in &frame.deltas {
            match delta {
                RowDelta::Added(row) | RowDelta::Updated(row) => {
                    self.by_id.insert(row.id(), row.clone());
                    touched = true;
                }
                RowDelta::Removed(id) => {
                    self.by_id.remove(id);
                    touched = true;
                }
                RowDelta::SourcesGrew { id, sources } => {
                    // The only delta that is a patch rather than a value. It
                    // is a no-op when we never saw the `Added` -- which
                    // happens, because a slow observer's deltas are rebased
                    // and conflated.
                    if let Some(row) = self.by_id.remove(id) {
                        let rebuilt = Row::from_parts(
                            row.id(),
                            row.pubkey(),
                            row.created_at(),
                            row.kind(),
                            row.tags().clone(),
                            row.content().to_string(),
                            row.signature(),
                            sources.clone(),
                        );
                        self.by_id.insert(*id, rebuilt);
                        touched = true;
                    }
                }
            }
        }
        if touched || before != self.by_id.len() {
            self.resort();
            self.dirty = true;
        }
    }

    /// Newest-first, ties broken by event id -- the order the WINDOWED mode
    /// documents and the unbounded mode does not deliver.
    fn resort(&mut self) {
        self.ordered = self.by_id.keys().copied().collect();
        let by_id = &self.by_id;
        self.ordered.sort_unstable_by(|left, right| {
            let (l, r) = (&by_id[left], &by_id[right]);
            r.created_at()
                .cmp(&l.created_at())
                .then_with(|| l.id().cmp(&r.id()))
        });
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// Newest-first rows, for rendering.
    pub fn rows(&self) -> impl Iterator<Item = &Row> {
        self.ordered.iter().map(move |id| &self.by_id[id])
    }

    #[must_use]
    pub fn get(&self, id: &EventId) -> Option<&Row> {
        self.by_id.get(id)
    }

    /// Take and clear the repaint flag.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::replace(&mut self.dirty, false)
    }

    /// Every distinct author currently in the table.
    ///
    /// Needed because rendering a row needs a profile, profiles are a
    /// SECOND query keyed by author, and nothing projects the authors of a
    /// live result set. See `profiles::ProfileBook`.
    pub fn authors(&self) -> Vec<PublicKey> {
        let mut authors: Vec<PublicKey> = self.by_id.values().map(Row::pubkey).collect();
        authors.sort_unstable();
        authors.dedup();
        authors
    }

    /// Insert a row this app minted locally, before any engine round trip.
    ///
    /// `composer::Composer` uses this for the optimistic post. Note what it
    /// costs: `Row::from_parts` is documented as "does not insert into NMP or
    /// claim provenance or signature validity", so this row is indistinguishable
    /// at the table level from an engine-delivered one, and when the engine's
    /// own optimistic row for the same write arrives it lands on the same id
    /// and silently replaces ours. That works, but only because we guessed the
    /// id right -- see `composer` for how we get it.
    pub fn insert_local(&mut self, row: Row) {
        self.by_id.insert(row.id(), row);
        self.resort();
        self.dirty = true;
    }
}

/// Everything a single timeline row needs to render, gathered from four
/// different places.
#[derive(Debug, Clone)]
pub struct RowView {
    pub id: EventId,
    pub author: PublicKey,
    /// From `profiles::ProfileBook`, a SEPARATE observation the app joins by
    /// hand on every repaint.
    pub author_display: Option<String>,
    pub created_at: u64,
    pub content: String,
    /// Parsed by `nmp-content` -- the one part of row rendering the surface
    /// actually hands over.
    pub blocks: usize,
    /// Read out of the row's own tags by this app, because nothing projects
    /// it. See `thread::depth_of`.
    pub reply_depth: Option<usize>,
    /// The room this row belongs to, if it carries an `h` row. Re-implemented
    /// here from raw `&[String]` tag cells -- `nmp-nip29` owns the meaning of
    /// `h` but exposes no "which group is this row in" reader.
    pub room: Option<String>,
    /// Provenance: which relays this app has seen this exact event at.
    pub sources: Vec<RelayUrl>,
    /// `false` while the signer has not answered. `Row::signed_event()`
    /// returns `None` for these, which is what makes a pending row
    /// un-reactable and un-repliable -- see `findings::PENDING_ROW_*`.
    pub signed: bool,
}

impl RowView {
    /// Build the view model for one row.
    ///
    /// The whole point of this function is how many different mechanisms it
    /// has to touch: `Row` accessors for the body, `Row::signature` for
    /// pending-ness, `nmp_content::parse_content` for the text, hand-written
    /// tag-cell scanning for `h`, and a caller-supplied profile lookup and
    /// depth, neither of which the row knows about.
    #[must_use]
    pub fn of(row: &Row, author_display: Option<String>, reply_depth: Option<usize>) -> Self {
        let document =
            nmp_content::parse_content(row.content(), nmp_content::ContentSyntax::PlainText);
        Self {
            id: row.id(),
            author: row.pubkey(),
            author_display,
            created_at: row.created_at().as_secs(),
            content: row.content().to_string(),
            blocks: document.blocks.len(),
            reply_depth,
            room: first_tag_value(row, "h"),
            sources: row.sources().iter().cloned().collect(),
            signed: matches!(row.signature(), nmp::RowSignature::Signed(_)),
        }
    }
}

/// Read the first value cell of the first tag row with this name.
///
/// Every app writes this. `Row::tags()` returns `nostr::Tags`, a type `nmp`
/// does NOT re-export, so the only way to walk it from an `nmp`-only app is
/// `.iter()` on the un-nameable value and `Tag::as_slice()` down to
/// `&[String]`. From there the app is re-implementing tag reading for every
/// NIP it renders -- `h` for NIP-29, `e`/`E` for NIP-10/NIP-22, `p` for
/// mentions, `d` for addressables.
#[must_use]
pub fn first_tag_value(row: &Row, name: &str) -> Option<String> {
    row.tags().iter().find_map(|tag| {
        let cells = tag.as_slice();
        (cells.first().map(String::as_str) == Some(name))
            .then(|| cells.get(1).cloned())
            .flatten()
    })
}

/// Every value cell of every tag row with this name.
#[must_use]
pub fn tag_values(row: &Row, name: &str) -> Vec<String> {
    row.tags()
        .iter()
        .filter_map(|tag| {
            let cells = tag.as_slice();
            (cells.first().map(String::as_str) == Some(name))
                .then(|| cells.get(1).cloned())
                .flatten()
        })
        .collect()
}
