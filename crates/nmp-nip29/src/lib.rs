//! `nmp-nip29` -- the [`Group`] door: NIP-29 group context over an explicitly
//! selected host, plus composers for the kinds NIP-29 itself defines (#989,
//! kinds 9000-9022: [`join_request`], [`leave_request`], [`add_user`],
//! [`remove_user`], [`edit_metadata`], [`delete_event`], [`create_group`],
//! [`delete_group`], [`create_invite`]).
//!
//! [`Group`] is the ONE app-facing door (#977). It is an identity --
//! `(host, group_id)`, constructed without contacting anything -- that mints
//! both halves of a group's traffic: a read `Demand` the app takes through the
//! ordinary one read door, and a `WriteIntent` carrying the `h` row this crate
//! owns and an explicit route to the host. The app never names the host for a
//! write, never spells a routing value, and never touches `h`.
//!
//! Everything here is PURE composition over `nostr` + `nmp-grammar`: no
//! engine, no signer, no resolver, no receipt. `nmp`'s own extension trait is
//! what hands a minted intent to the one publish door, which is why the
//! dependency runs `nmp -> nmp-nip29` and never the other way
//! (`scripts/check-nip29-ownership.sh`).
//!
//! This crate defines neither kind:10009's schema (owned exclusively by
//! `nmp-nip51`, which this crate does not depend on at all -- #858) nor the
//! schema of any event that is merely *published into* a group rather than
//! defined by NIP-29 itself. NIP-C7 kind:9 chat belongs to `nmp-nipc7`;
//! mention and notification policy belongs to the client/content layer
//! (#838).
//!
//! #858's boundary: there is no NIP-29 re-labelling of the NIP-51 Simple-
//! groups value. An app that has decoded a kind:10009 list reads
//! `nmp_nip51::SimpleGroupEntry` AS ITSELF and passes the exact fields it
//! chose -- a host `RelayUrl`, a `group_id` string -- into [`Group::new`] or
//! [`group_discovery_demand`]. Nothing here copies, renames, or re-owns that
//! schema's decode result. [`Group`] contributes only the NIP-29-owned `h`
//! tag and the route to the selected host -- "contextual publication is not
//! kind ownership".
//!
//! `previous` is deliberately absent. It remains omitted until a host-scoped,
//! group-scoped, author-aware live-window capability can mint it without
//! caller tuples, silent truncation, or transplantation (#838). A draft that
//! arrives carrying one is a typed refusal, never a trimmed draft.
//!
//! Non-goals (mirrors #108's issue text exactly): no kind:30002 semantics;
//! no `rememberGroup`/`forgetGroup` mutation (gated on #50).

mod demand;
mod group;
mod operations;

pub use demand::group_discovery_demand;
pub use group::{Group, GroupContextError};
pub use operations::{
    add_user, create_group, create_invite, delete_event, delete_group, edit_metadata, join_request,
    leave_request, remove_user,
};
