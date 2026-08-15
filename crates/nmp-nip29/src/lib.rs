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
//! - [`current_account_group_list_demand`] and the tolerant kind:10009
//!   Simple-groups codec used to select a remembered NIP-29 host.
//! - the kinds NIP-29 itself defines (#989, 9000-9022: [`join_request`],
//!   [`leave_request`], [`add_users`], [`remove_users`], [`edit_metadata`],
//!   [`delete_event`], [`create_group`], [`delete_group`], [`create_invite`]).
//!
//! Everything here is PURE composition over `nostr` + `nmp-grammar`: no
//! engine, no signer, no resolver, no receipt, no routing, and no
//! `WriteIntent`. The APP-FACING door -- `nmp::nip29::on(hosts)`, the relay
//! scope it returns, and the group that narrows it -- lives in the `nmp`
//! facade, because that door must retain a relay scope AND mint the one
//! opaque write intent, and a lower crate cannot do the second without
//! importing the write plane. The dependency therefore runs
//! `nmp -> nmp-nip29` and never the other way.
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
//! The kind:10009 read is deliberately observational: its tolerant result
//! grants no signature, canonical-store, routing, or mutation authority.
//! The schema of any event merely *published into* a group remains outside
//! this crate. NIP-C7 kind:9 chat belongs to `nmp-nipc7`; mention and
//! notification policy belongs to the client/content layer (#838).
//! Contextual publication is not kind ownership.
//!
//! The typed kind:10009 add/remove operations live one layer up at
//! `nmp::nip29`: they need the engine's durable semantic-operation and receipt
//! lifecycle, while this crate remains pure schema and composition. The
//! dependency therefore stays `nmp -> nmp-nip29`.
//!
//! `previous` is deliberately absent. It remains omitted until a host-scoped,
//! group-scoped, author-aware live-window capability can mint it without
//! caller tuples, silent truncation, or transplantation (#838). A draft that
//! arrives carrying one is a typed refusal, never a trimmed draft.
//!
//! Non-goal (mirrors #108's issue text exactly): no kind:30002 semantics.

mod context;
mod discovery;
mod group_list;
mod operations;
mod records;
mod simple_groups;

pub use context::{contextualize, group_demand_at, validate_context, GroupContextError};
pub use discovery::{
    admin_list_includes_at, groups_whose_record_matches_at, member_list_includes_at,
    GROUP_ADMINS_KIND, GROUP_MEMBERS_KIND, GROUP_METADATA_KIND,
};
pub use group_list::current_account_group_list_demand;
pub use operations::{
    add_users, create_group, create_invite, delete_event, delete_group, edit_metadata,
    join_request, leave_request, remove_users, GroupMetadataEdit, GroupUser, GroupUsersError,
    JoinAccess, ReadAccess,
};
pub use records::{
    group_metadata_at, group_records_at, join_key_of, listed_record_at, GroupMetadata, GroupRecord,
    ListedRecord, ListedSubject,
};
pub use simple_groups::{
    parse_simple_groups_list_from_raw_tags_tolerant, parse_simple_groups_list_tolerant,
    SimpleGroupEntry, SimpleGroupsList,
};
