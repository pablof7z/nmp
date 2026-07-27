//! `nmp-nip29` -- NIP-29 host browsing (read, #63/#108) and send-into-group
//! composition (write, #115), both routed through an explicitly selected
//! host relay. This crate DEFINES neither kind:10009's schema (owned
//! exclusively by `nmp-nip51`, which this crate does not depend on at all --
//! #858) nor kinds 9/39000/30315 -- kind 30315 in particular is NIP-38 "user
//! statuses", a DIFFERENT protocol this crate reads cross-NIP without
//! defining.
//!
//! #858's boundary: there is no NIP-29 re-labelling of the NIP-51 Simple-
//! groups value. An app that has decoded a kind:10009 list reads
//! `nmp_nip51::SimpleGroupEntry` AS ITSELF and passes the exact fields it
//! chose -- a host `RelayUrl`, a `group_id` string -- into
//! [`group_discovery_demand`]/[`group_content_demand`]. Nothing here copies,
//! renames, or re-owns that foreign schema's decode result.
//! `compose_group_send` (#115) contributes an `h` tag/host authority to a
//! draft whose schema it does not define -- "contextual publication is not
//! kind ownership" (`docs/design/routing-and-ownership.md` §3.2.1).
//!
//! Non-goals (mirrors #108's issue text exactly): no kind:30002 semantics;
//! no `rememberGroup`/`forgetGroup` mutation (gated on #50).

mod demand;
#[cfg(feature = "engine")]
mod message;
mod send;

pub use demand::{group_content_demand, group_discovery_demand};
#[cfg(feature = "engine")]
pub use message::{compose_group_message, GroupMessageError, GroupReplyParent};
pub use send::{compose_group_send, GroupSendError, GroupTimelineEvidence, PREVIOUS_MAX};
