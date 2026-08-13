//! NIP-51 kind:10009 simple-groups lists projected through the canonical
//! facade (#1239).
//!
//! `nmp-nip51` owns the kind:10009 schema, its tolerant codec, and the demand
//! that reads the current account's list. This module re-exports that
//! vocabulary so a direct-Rust NIP-29 client can answer "which groups is this
//! account in" through `nmp` alone, the way a Swift app already does through
//! `nmp-ffi`.
//!
//! Parsing here stays TOLERANT and OBSERVATIONAL (#863): what comes back is
//! plain data with no signature, provenance, routing or mutation authority.
//! Re-exporting it through the facade changes nothing about that -- the hosts
//! an app browses a group with remain its own explicit typed input to
//! [`crate::nip29::on`], never harvested from parser output.
//!
//! [`current_account_demand`] is an ordinary [`crate::Demand`], so the read is
//! an ordinary [`crate::LiveQuery`]; no observation handle, frame proof or
//! qualified "observed" wrapper exists here to project.

pub use nmp_nip51::{
    current_account_demand, parse_simple_groups_list_from_raw_tags_tolerant,
    parse_simple_groups_list_tolerant, SimpleGroupEntry, SimpleGroupsList,
};
