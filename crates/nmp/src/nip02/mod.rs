//! `nmp::nip02` -- the app-facing NIP-02 follow door (#1143).
//!
//! # Why the door lives here and not in `nmp-nip02`
//!
//! `nmp-nip02` is engine-free by construction -- `nostr` + `nmp-grammar`, no
//! core, no mechanism. It owns the one thing that needs neither: the
//! reactive kind:3 demand ([`current_account_demand`](nmp_nip02::current_account_demand)).
//! It never imports, constructs, or returns a [`WriteIntent`](crate::WriteIntent)
//! or a [`Row`](crate::Row).
//!
//! Composing a follow/unfollow write and observing the relationship both
//! need the engine itself -- minting the ordinary `WriteIntent`, freezing
//! the selected account, entering the receipt lifecycle, folding a live
//! query into a snapshot. A lower crate cannot do any of that without a
//! reverse dependency on `nmp`, which is exactly the defect #1143 records:
//! before this module existed, `nmp-nip02` depended on `nmp` to reach these
//! nouns, the only upward edge in the workspace's dependency graph, which
//! meant NIP-02 reached apps only through the FFI layer and never through
//! the direct-Rust facade. So the door is here, the vocabulary is below,
//! and the dependency runs `nmp -> nmp-nip02` only -- the identical shape
//! `nmp::nip29`'s door already uses.

mod observe;
mod writes;

pub use observe::{
    observe_following, observe_following_async, set_following, AsyncFollowObservation,
    FollowActionFailure, FollowAvailability, FollowObservation, FollowRelationship, FollowSnapshot,
};
pub use writes::{follow_capability, follow_writes, follows, FollowChange, FollowWrites};

// The pure reactive demand stays owned by `nmp-nip02` -- re-exported here so
// an app that only names `nmp` still reaches it without a second `use` line,
// the same convenience `nmp::nip29` gives its own pure vocabulary crate.
pub use nmp_nip02::current_account_demand;
