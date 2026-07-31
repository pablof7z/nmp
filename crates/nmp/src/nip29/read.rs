//! The one place a NIP-29 read becomes a live query (#1033, consuming #1108).
//!
//! Every NIP-29 read is a set of complete, independent, singleton-host
//! `Demand` branches -- one per host in the scope, each binding exactly one
//! normalized host to its own selection plus its own complete source
//! authority, access context, cache mode and freshness policy. Host A's
//! filter can neither constrain nor authorize host B.
//!
//! Those branches become ONE ordinary [`LiveQuery`] observed through the one
//! ordinary observe/subscription/frame lifecycle:
//!
//! ```text
//! one host   -> a single complete branch
//! many hosts -> the canonical nonempty set of complete branches
//! ```
//!
//! Never `Pinned({A, B})` -- that cross-products a value derived at A onto B.
//! Never `Vec<Demand>` handed to the app -- that moves coherent row, evidence,
//! window and cancellation ownership into every app. Never a NIP-29-specific
//! observe door.
//!
//! This module is deliberately one small file: it is the ENTIRE seam onto
//! #1108's composite live-query shape.

use nmp_grammar::{Demand, LiveQuery, LiveQueryError};

use crate::nip29::GroupContextError;

/// Why a NIP-29 read produced no live query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupReadError {
    /// The app's own selection collided with a row the group owns.
    Context(GroupContextError),
    /// The branches could not form one live query -- in practice, a scope
    /// naming more hosts than one observation supports
    /// (`LiveQuery::MAX_BRANCHES`).
    ///
    /// A refusal, never a degraded answer: collapsing branches into one
    /// `Pinned` set would return a confidently wrong cross-product, and
    /// dropping branches would silently under-resolve.
    Declaration(LiveQueryError),
}

impl From<GroupContextError> for GroupReadError {
    fn from(error: GroupContextError) -> Self {
        Self::Context(error)
    }
}

impl From<LiveQueryError> for GroupReadError {
    fn from(error: LiveQueryError) -> Self {
        Self::Declaration(error)
    }
}

impl std::fmt::Display for GroupReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Context(error) => write!(f, "{error}"),
            Self::Declaration(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for GroupReadError {}

/// Fold one complete branch per host into ONE live query.
///
/// `branches` is always nonempty: it is built by mapping over a
/// [`RelayScope`](crate::nip29::RelayScope)'s hosts, and a scope cannot be
/// empty.
pub(crate) fn one_live_query(branches: Vec<Demand>) -> Result<LiveQuery, GroupReadError> {
    let mut branches = branches;
    if branches.len() == 1 {
        return Ok(LiveQuery::single(
            branches.pop().expect("exactly one branch"),
        ));
    }
    // No aggregate row bound: a NIP-29 listing bounds nothing globally, and
    // inventing one here would silently cap what the app asked for.
    LiveQuery::union(branches.into_iter().map(LiveQuery::single), None)
        .map_err(GroupReadError::Declaration)
}
