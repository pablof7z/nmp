//! `nmp-nip51` -- the exclusive schema owner, codec, and query home for
//! NIP-51 kind:10009 (Simple groups list, #63/#108). No NIP-29 behavior
//! lives here (#63's ownership boundary): `nmp-nip29` is the optional,
//! one-directional consumer of this crate's typed output, never the other
//! way around.
//!
//! Whole-list edits use NMP's source-scoped exact-base contract: the module
//! preserves every unrelated tag and encrypted content byte-for-byte, and
//! native callers can only request semantic add/remove operations.

mod claims;
mod demand;
mod edit;
mod service;
mod simple_groups;

pub use claims::claims;
pub use demand::active_account_demand;
pub use edit::{
    compose_relay_change, contains_relay, ComposeRelayChangeError, ComposeRelayChangeResult,
    RelayChange,
};
pub use service::{
    prepare_set_relay, set_relay, RelayAction, RelayActionFailure, RelayActionRunner,
    RelayActionStatus,
};
pub use simple_groups::{
    decode_simple_groups_list, decode_simple_groups_list_from_raw_tags, SimpleGroupEntry,
    SimpleGroupsList,
};
