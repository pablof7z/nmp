//! `nmp-nip29` -- NIP-29's VOCABULARY: the schema NIP-29 itself defines, the
//! `h` context row's semantics, and the relationships between the three
//! relay-signed kinds that describe a group.
//!
//! This crate mints atomic, complete values and nothing else:
//!
//! - [`contextualize`] / [`validate_context`] -- the `h` row an event carries,
//!   appended to an unsigned draft or validated on an already-signed event.
//! - [`group_demand_at`] -- one host's complete read branch for one group's
//!   CONTENT. It refuses the three relay-signed records, which are `d`-keyed
//!   and unreachable through the `h` axis (#1245).
//! - [`member_list_includes_at`] / [`admin_list_includes_at`] /
//!   [`group_records_at`] -- one host's complete discovery branch, built from
//!   the relationships between kinds 39000/39001/39002 and the `d` join key.
//! - [`group_metadata_at`] / [`listed_record_at`] -- what one of those
//!   relay-signed records SAYS, read once here rather than four times in two
//!   applications (#1233).
//! - the kinds NIP-29 itself defines (#989, 9000-9022: [`join_request`],
//!   [`leave_request`], [`add_user`], [`remove_user`], [`edit_metadata`],
//!   [`delete_event`], [`create_group`], [`delete_group`], [`create_invite`]).
//!
//! Everything here is PURE composition over `nostr` + `nmp-grammar`: no
//! engine, no signer, no resolver, no receipt, no routing, and no
//! `WriteIntent`. The APP-FACING door -- `nmp::nip29::on(hosts)`, the relay
//! scope it returns, and the group that narrows it -- lives in the `nmp`
//! facade, because that door must retain a relay scope AND mint the one
//! opaque write intent, and a lower crate cannot do the second without
//! importing the write plane. The dependency therefore runs
//! `nmp -> nmp-nip29` and never the other way
//! (`scripts/check-nip29-ownership.sh`).
//!
//! # One host per value
//!
//! NIP-29 authority is per-relay: the `h` tag is a label and the relay
//! decides, so two relays hosting the same group id are two independent
//! groups with the same name. Every read constructor here takes exactly ONE
//! host and stamps it explicitly at every level it owns. Assembling one
//! branch per host into a single live query belongs to the facade; this crate
//! never sees a relay SET and therefore never has an empty one to refuse.
//!
//! # What this crate does not own
//!
//! Neither kind:10009's schema (owned exclusively by `nmp-nip51`, which this
//! crate does not depend on at all -- #858) nor the schema of any event that
//! is merely *published into* a group rather than defined by NIP-29 itself.
//! NIP-C7 kind:9 chat belongs to `nmp-nipc7`; mention and notification policy
//! belongs to the client/content layer (#838). Contextual publication is not
//! kind ownership.
//!
//! `previous` is deliberately absent. It remains omitted until a host-scoped,
//! group-scoped, author-aware live-window capability can mint it without
//! caller tuples, silent truncation, or transplantation (#838). A draft that
//! arrives carrying one is a typed refusal, never a trimmed draft.
//!
//! Non-goals (mirrors #108's issue text exactly): no kind:30002 semantics;
//! no `rememberGroup`/`forgetGroup` mutation (gated on #50).

mod context;
mod discovery;
mod operations;
mod records;

pub use context::{contextualize, group_demand_at, validate_context, GroupContextError};
pub use discovery::{
    admin_list_includes_at, member_list_includes_at, GROUP_ADMINS_KIND, GROUP_MEMBERS_KIND,
    GROUP_METADATA_KIND,
};
pub use operations::{
    add_user, create_group, create_invite, delete_event, delete_group, edit_metadata, join_request,
    leave_request, remove_user,
};
pub use records::{
    group_metadata_at, group_records_at, join_key_of, listed_record_at, GroupMetadata, GroupRecord,
    ListedRecord, ListedSubject,
};
