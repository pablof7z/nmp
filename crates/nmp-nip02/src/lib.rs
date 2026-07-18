//! Optional NIP-02 support: one schema owner for kind:3, one ordinary live
//! demand, one exact tag-preserving editor, and an NMP-owned following
//! resource/action. UI packages consume the projected state and invoke the
//! action; they never parse contact lists or manufacture replacement events.

mod claims;
mod demand;
mod edit;
mod service;

pub use claims::claims;
pub use demand::active_account_demand;
pub use edit::{
    compose_follow_change, expected_base, follows, ComposeFollowError, ComposeFollowResult,
    FollowChange,
};
pub use service::{
    observe_following, observe_following_async, prepare_set_following, set_following,
    AsyncFollowObservation, FollowAction, FollowActionFailure, FollowActionRunner,
    FollowActionStatus, FollowAvailability, FollowObservation, FollowRelationship, FollowSnapshot,
};
