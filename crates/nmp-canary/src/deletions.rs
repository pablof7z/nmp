//! NIP-09 deletions: the kind nobody owns, that every screen must subscribe
//! and then hide.
//!
//! ## What we wanted to write
//!
//! ```text
//! let feed = engine.observe(follows_feed(), window)?;   // deletions applied
//! ```
//!
//! ## What we wrote
//!
//! Three things, all of them the app's.
//!
//! ### 1. The app must widen its own filter
//!
//! `nmp-store` processes kind:5 inside `insert` and writes PERMANENT
//! tombstones (`crates/nmp-store/src/lib.rs:20`), so a deletion that ARRIVES
//! is applied correctly and the target leaves every matching query as a
//! `RowDelta::Removed`. The engine will not ask for one: nothing in
//! `nmp-router` or `nmp-resolver` widens a demand's `kinds` to include 5
//! (searched: `git grep -rn "EventDeletion\|kinds.insert(5\|kind == 5" --
//! crates/nmp-router/src crates/nmp-resolver/src crates/nmp-engine/src`
//! returns nothing). A feed declared as `kinds: [1, 6]` therefore never sees a
//! deletion for any row in it, on any relay, ever.
//!
//! So [`with_deletions`] exists, and every query in this app goes through it.
//!
//! ### 2. The app must then hide what it just asked for
//!
//! kind:5 is "stored normally like any other regular event"
//! (`crates/nmp-store/src/lib.rs:496`), so widening the filter puts deletion
//! events INTO the row set the timeline renders. [`is_deletion`] and
//! [`display_rows`] are the filter back out. The app asks for rows it must not
//! show, in order to get an effect it cannot otherwise get.
//!
//! That is two app-side steps for one engine behaviour, and both are silent
//! failures: forget the first and deletions never apply; forget the second and
//! empty grey rows appear in the timeline. Neither shows up as an error.
//!
//! ### 3. There is no NIP-09 crate
//!
//! `crates/` holds nip02, nip11, nip18, nip22, nip25, nip29, nip65, nip73,
//! nipc7, bookmarks and content. No nip09. So composing a deletion is
//! [`delete`] below: `EventBuilder::new(Kind::from(5))` plus hand-built `e`
//! and `k` rows. NIP-09 says a deletion SHOULD carry a `k` row naming the
//! deleted kind; nothing here enforces or supplies that, so an app gets it
//! right by having read the NIP.
//!
//! Note what is NOT missing: the deletion is a write like any other, and the
//! store's own tombstone handling is thorough enough to have a documented
//! design for the pending-draft case (a locally-composed kind:5 suppresses
//! optimistically and commits permanently only on promotion). The mechanism is
//! good. The vocabulary in front of it does not exist.

use std::collections::BTreeSet;

use nmp::{
    Engine, EventBuilder, EventId, Identity, Kind, PublicKey, ReceiptStream, Row, Tag, WriteIntent,
    WritePayload, WriteRouting,
};

/// NIP-09 kind:5. Declared here because no crate in this workspace owns it.
pub const DELETION_KIND: u16 = 5;

/// Add kind:5 to a kind set, so deletions for these rows can arrive at all.
///
/// Every `Filter::kinds` in this app is built through here. That is the whole
/// mitigation available: a wrapper an app has to remember to call.
#[must_use]
pub fn with_deletions(kinds: impl IntoIterator<Item = u16>) -> BTreeSet<u16> {
    let mut kinds: BTreeSet<u16> = kinds.into_iter().collect();
    kinds.insert(DELETION_KIND);
    kinds
}

/// Is this row a deletion the app asked for and must not render?
#[must_use]
pub fn is_deletion(row: &Row) -> bool {
    row.kind().as_u16() == DELETION_KIND
}

/// The rows a timeline actually shows.
pub fn display_rows<'a>(rows: impl Iterator<Item = &'a Row>) -> impl Iterator<Item = &'a Row> {
    rows.filter(|row| !is_deletion(row))
}

/// The event ids a deletion row names.
///
/// Hand-read from `e` cells, like every other tag read in this app. Used only
/// to explain a disappearance in the UI ("this was deleted") -- the ROW
/// removal itself is the store's, and correct.
#[must_use]
pub fn targets_of(deletion: &Row) -> Vec<EventId> {
    crate::rows::tag_values(deletion, "e")
        .into_iter()
        .filter_map(|hex| EventId::from_hex(&hex).ok())
        .collect()
}

/// Compose and publish a NIP-09 deletion. No capability crate owns this.
pub fn delete(
    engine: &Engine,
    author: PublicKey,
    target: &Row,
    reason: &str,
) -> Result<ReceiptStream, nmp::EngineError> {
    let mut builder = EventBuilder::new(Kind::from(DELETION_KIND)).content(reason);
    builder = builder
        .tag(Tag::parse(["e", &target.id().to_hex()]).expect("a two-cell e row is well formed"));
    // NIP-09's `k` row. Supplied because the spec asks for it, not because
    // anything here would notice its absence.
    builder = builder.tag(
        Tag::parse(["k", &target.kind().as_u16().to_string()])
            .expect("a two-cell k row is well formed"),
    );
    engine.publish(WriteIntent {
        payload: WritePayload::Event(builder),
        routing: WriteRouting::Auto,
        identity: Identity::Explicit(author),
    })
}
