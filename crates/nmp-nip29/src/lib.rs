//! `nmp-nip29` -- NIP-29 host browsing (read, #63/#108) and send-into-group
//! composition (write, #115), both routed through an explicitly selected
//! host relay. This crate DEFINES neither kind:10009's schema (defined by
//! `nmp-nip51`, consumed here through that crate's typed output) nor kinds
//! 9/39000/30315 -- kind 30315 in particular is NIP-38 "user statuses", a
//! DIFFERENT protocol this crate reads cross-NIP without defining.
//! `compose_group_send` (#115) contributes an `h` tag/host authority to a
//! draft whose schema it does not define -- "contextual publication is not
//! kind ownership" (`docs/design/routing-and-ownership.md` §3.2.1).
//!
//! Non-goals (mirrors #108's issue text exactly): no kind:30002 semantics;
//! no `rememberGroup`/`forgetGroup` mutation (gated on #50).

mod demand;
mod group_ref;
#[cfg(feature = "engine")]
mod message;
mod send;

pub use demand::{group_content_demand, group_discovery_demand};
pub use group_ref::{remembered_groups, GroupRef, RememberedGroups};
#[cfg(feature = "engine")]
pub use message::{compose_group_message, GroupMessageError, GroupReplyParent};
pub use send::{compose_group_send, GroupSendError, GroupTimelineEvidence, PREVIOUS_MAX};
