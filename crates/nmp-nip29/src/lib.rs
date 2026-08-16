//! `nmp-nip29` -- ALL of NIP-29: the schema NIP-29 itself defines, the `h`
//! context row's semantics, the relationships between the three relay-signed
//! kinds that describe a group, and the app-facing relay-scope/group door
//! that mints the one opaque write intent (#1707 reversed #1033's absorption
//! of that door into `nmp`).
//!
//! This crate mints atomic, complete values and, for the door half, composes
//! `nmp`'s own engine surface to publish and observe them:
//!
//! - [`on`] / [`RelayScope`] / [`group`] -- name the relays a group lives on,
//!   ONCE, and narrow to one group id or the several a write belongs to
//!   ([`Groups`], #1281).
//! - [`Group`] -- one group id within a scope: [`Group::read`] mints the
//!   `LiveQuery` the ordinary observe door takes, [`Group::observe`] watches
//!   NIP-29's own relay-signed records, and [`Group::publish`] (plus the
//!   named 9000-9022 operations) is the group's ONE write door.
//! - [`group_list_capability`] / [`group_list_writes`] -- the durable
//!   kind:10009 saved-groups list operations, entering the engine's
//!   receipt/semantic-write lifecycle exactly like every other capability.
//! - [`contextualize`] / [`validate_context`] -- the `h` row an event carries,
//!   appended to an unsigned draft or validated on an already-signed event.
//! - [`group_demand_at`] -- one host's complete read branch for one group's
//!   CONTENT. It refuses the three relay-signed records, which are `d`-keyed
//!   and unreachable through the `h` axis (#1245).
//! - [`member_list_includes`] / [`admin_list_includes`] / [`groups_whose_record_matches`] /
//!   [`all`] -- which groups a records observation covers (#1252), and
//!   [`member_list_includes_at`] / [`admin_list_includes_at`] /
//!   [`group_records_at`] -- the same, lowered at one host.
//! - [`group_metadata_at`] / [`listed_record_at`] -- what one of those
//!   relay-signed records SAYS, read once here rather than four times in two
//!   applications (#1233).
//! - [`current_account_group_list_demand`] and the tolerant kind:10009
//!   Simple-groups codec used to select a remembered NIP-29 host.
//! - the kinds NIP-29 itself defines (#989, 9000-9022: [`join_request`],
//!   [`leave_request`], [`add_users`], [`remove_users`], [`edit_metadata`],
//!   [`delete_event`], [`create_group`], [`delete_group`], [`create_invite`]).
//!
//! The pure schema/predicate half needs only `nostr` + `nmp-grammar`; the
//! door half ([`scope`], [`group`](mod@group), [`groups`],
//! [`group_list_writes`](mod@group_list_writes), [`record_observation`])
//! needs `nmp`'s own engine surface (`WriteIntent`, `Row`, receipt custody,
//! live-query folding) to compose a durable operation or open an
//! observation -- both live in this one crate regardless.
//!
//! The dependency runs `nmp-nip29 -> nmp`, the ordinary shape of a
//! capability crate composing the engine it runs against, not an inversion:
//! `nmp` never names `nmp-nip29` back. `nmp` must not know what a NIP-29
//! group or a kind:10009 saved-groups list means.
//!
//! # One host per value, below the door
//!
//! NIP-29 authority is per-relay: the `h` tag is a label and the relay
//! decides, so two relays hosting the same group id are two independent
//! groups with the same name. Every `_at` read constructor here takes exactly
//! ONE host and stamps it explicitly at every level it owns; [`RelayScope`]
//! is where those per-host branches become one ordinary live query over a
//! caller-named SET.
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
//! `previous` is deliberately absent. It remains omitted until a host-scoped,
//! group-scoped, author-aware live-window capability can mint it without
//! caller tuples, silent truncation, or transplantation (#838). A draft that
//! arrives carrying one is a typed refusal, never a trimmed draft.
//!
//! Non-goal (mirrors #108's issue text exactly): no kind:30002 semantics.

mod context;
mod discovery;
mod group;
mod group_list;
mod group_list_writes;
mod groups;
mod operations;
mod predicate;
mod read;
mod record_observation;
mod records;
mod scope;
mod simple_groups;

pub use context::{contextualize, group_demand_at, validate_context, GroupContextError};
pub use discovery::{
    admin_list_includes_at, groups_whose_record_matches_at, member_list_includes_at,
    GROUP_ADMINS_KIND, GROUP_MEMBERS_KIND, GROUP_METADATA_KIND,
};
pub use group::{Group, GroupPublishError};
pub use group_list::current_account_group_list_demand;
pub use group_list_writes::{
    add_group_to_list, add_relay_in_use, group_list_capability, group_list_writes,
    remove_group_from_list, remove_relay_in_use, GroupListActionError, GroupListWrites,
};
pub use groups::Groups;
pub use operations::{
    add_users, create_group, create_invite, delete_event, delete_group, edit_metadata,
    join_request, leave_request, remove_users, GroupMetadataEdit, GroupUser, GroupUsersError,
    JoinAccess, ReadAccess,
};
pub use predicate::{
    admin_list_includes, all, any_of, groups_whose_record_matches, member_list_includes, GroupIds,
    GroupPredicate, GroupPredicateError,
};
pub use read::GroupReadError;
pub use record_observation::{
    GroupAvailability, GroupObservation, GroupObserveError, GroupSnapshot, GroupWaitError,
    HostRecords,
};
pub use records::{
    group_metadata_at, group_records_at, join_key_of, listed_record_at, GroupMetadata, GroupRecord,
    ListedRecord, ListedSubject,
};
pub use scope::{group, on, RelayScope, RelayScopeError};
pub use simple_groups::{
    parse_simple_groups_list_from_raw_tags_tolerant, parse_simple_groups_list_tolerant,
    SimpleGroupEntry, SimpleGroupsList,
};
