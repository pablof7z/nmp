//! `nmp-nip29` -- NIP-29 group context over an explicitly selected host.
//! This crate defines neither kind:10009's schema (owned exclusively by
//! `nmp-nip51`, which this crate does not depend on at all -- #858) nor the
//! schema of any event published into a group. NIP-C7 kind:9 chat belongs to
//! `nmp-nipc7`; mention and notification policy belongs to the client/content
//! layer (#838).
//!
//! #858's boundary: there is no NIP-29 re-labelling of the NIP-51 Simple-
//! groups value. An app that has decoded a kind:10009 list reads
//! `nmp_nip51::SimpleGroupEntry` AS ITSELF and passes the exact fields it
//! chose -- a host `RelayUrl`, a `group_id` string -- into
//! [`group_discovery_demand`] or [`contextualize_group_event`]. Nothing here
//! copies, renames, or re-owns that foreign schema's decode result.
//! `contextualize_group_event` contributes only the NIP-29-owned `h` tag and
//! retains the selected host alongside a complete foreign-schema draft --
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
mod publication;

pub use demand::group_discovery_demand;
pub use publication::{contextualize_group_event, GroupContextError, GroupPublication};
