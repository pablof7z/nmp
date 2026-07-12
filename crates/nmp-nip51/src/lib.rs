//! `nmp-nip51` -- the exclusive schema owner, codec, and query home for
//! NIP-51 kind:10009 (Simple groups list, #63/#108). No NIP-29 behavior
//! lives here (#63's ownership boundary): `nmp-nip29` is the optional,
//! one-directional consumer of this crate's typed output, never the other
//! way around.
//!
//! Read/decode-only in this crate today: `rememberGroup`/`forgetGroup`
//! replacement-write encoding stays gated on #50's source-scoped
//! base-version contract and is out of scope here.

mod claims;
mod demand;
mod simple_groups;

pub use claims::claims;
pub use demand::active_account_demand;
pub use simple_groups::{decode_simple_groups_list, SimpleGroupEntry, SimpleGroupsList};
