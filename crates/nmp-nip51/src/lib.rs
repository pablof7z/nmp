//! `nmp-nip51` -- the exclusive schema owner, codec, and query home for
//! NIP-51 kind:10009 (Simple groups list, #63/#108). No NIP-29 behavior
//! lives here (#63's ownership boundary), and since #858 nothing wraps this
//! crate's value either: `nmp-nip29` does not depend on `nmp-nip51`. An app
//! decodes the list here and passes the exact fields it selected (a host
//! `RelayUrl`, a `group_id`) into NIP-29's host-pinned demand constructors.
//!
//! Read/parse-only in this crate today: `rememberGroup`/`forgetGroup`
//! replacement-write encoding stays gated on #50's source-scoped
//! base-version contract and is out of scope here.
//!
//! Parsing here is TOLERANT and OBSERVATIONAL (#863). The exported
//! `parse_simple_groups_list_tolerant`/
//! `parse_simple_groups_list_from_raw_tags_tolerant` accept untrusted input
//! and return plain data -- no signature, canonical-store, provenance,
//! routing, or mutation authority. The kind:10009 read stays an ordinary
//! `Demand`/`LiveQuery`; this crate exports no observation handle, frame
//! proof, witness, or qualified "observed" wrapper, and
//! `scripts/check-nip51-no-derived-authority.sh` fails the build if one
//! reappears.

mod demand;
mod simple_groups;

pub use demand::current_account_demand;
pub use simple_groups::{
    parse_simple_groups_list_from_raw_tags_tolerant, parse_simple_groups_list_tolerant,
    SimpleGroupEntry, SimpleGroupsList,
};
