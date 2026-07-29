//! `nmp-nip29` -- NIP-29 group context over an explicitly selected host, plus
//! composers for the kinds NIP-29 itself defines (#989, kinds 9000-9022:
//! [`join_request`], [`leave_request`], [`add_user`], [`remove_user`],
//! [`edit_metadata`], [`delete_event`], [`create_group`], [`delete_group`],
//! [`create_invite`]). This crate defines neither kind:10009's schema (owned
//! exclusively by `nmp-nip51`, which this crate does not depend on at all --
//! #858) nor the schema of any event that is merely *published into* a group
//! rather than defined by NIP-29 itself. NIP-C7 kind:9 chat belongs to
//! `nmp-nipc7`; mention and notification policy belongs to the client/content
//! layer (#838).
//!
//! #858's boundary: there is no NIP-29 re-labelling of the NIP-51 Simple-
//! groups value. An app that has decoded a kind:10009 list reads
//! `nmp_nip51::SimpleGroupEntry` AS ITSELF and passes the exact fields it
//! chose -- a host `RelayUrl`, a `group_id` string -- into
//! [`group_discovery_demand`] or [`contextualize_group_event`]. Nothing here
//! copies, renames, or re-owns that schema's decode result.
//! `contextualize_group_event` contributes only the NIP-29-owned `h` tag and
//! retains the selected host alongside a complete draft --
//! "contextual publication is not kind ownership"
//! (`docs/design/routing-and-ownership.md` §3.2.1).
//!
//! `previous` is deliberately absent. It remains omitted until a host-scoped,
//! group-scoped, author-aware live-window capability can mint it without
//! caller tuples, silent truncation, or transplantation (#838).
//!
//! Non-goals (mirrors #108's issue text exactly): no kind:30002 semantics;
//! no `rememberGroup`/`forgetGroup` mutation (gated on #50).

mod demand;
mod operations;
mod publication;

pub use demand::group_discovery_demand;
pub use operations::{
    add_user, create_group, create_invite, delete_event, delete_group, edit_metadata, join_request,
    leave_request, remove_user,
};
pub use publication::{contextualize_group_event, GroupContextError, GroupPublication};
