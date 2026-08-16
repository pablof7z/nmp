//! NIP-51's kind:10003 public bookmarks list: the reactive demand that
//! reads it, and the durable add/remove operations over it.
//!
//! Written as #1715's proof that a new capability is addable without
//! touching `nmp`, `nmp-engine`, or `nmp-runtime` -- see this crate's own
//! `Cargo.toml` for the manifest half of that claim. Same
//! capability-owns-its-meaning shape `nmp-nip02` and `nmp-nip29` already
//! use: [`current_account_bookmarks_demand`] needs only `nostr` +
//! `nmp-grammar`; [`bookmark_capability`]/[`add_bookmark`]/
//! [`remove_bookmark`] compose the ordinary `WriteIntent` and enter the
//! engine's receipt lifecycle over `nmp`'s own public surface.
//!
//! Public bookmarks only -- see [`items`]'s own doc for why the private
//! (NIP-44-encrypted) half is deliberately out of scope.

mod items;
mod writes;

pub use items::{
    current_account_bookmarks_demand, parse_bookmarks_tolerant, BookmarkedItem, BookmarksList,
    BOOKMARKS_KIND,
};
pub use writes::{
    add_bookmark, bookmark_capability, bookmark_writes, remove_bookmark, BookmarkActionError,
    BookmarkWrites,
};
