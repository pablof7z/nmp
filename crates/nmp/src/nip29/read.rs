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
//! This module is deliberately one small file: it is the ENTIRE seam between
//! this issue and #1108's composite live-query shape. When #1108 lands,
//! [`one_live_query`] folds the branches with its union constructor and
//! [`GroupReadError::MultiHostReadRequiresUnionQuery`] is deleted outright.

use nmp_grammar::Demand;

use crate::nip29::GroupContextError;
use crate::LiveQuery;

/// Why a NIP-29 read produced no live query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupReadError {
    /// The app's own selection collided with a row the group owns.
    Context(GroupContextError),
    /// The scope names several hosts, so the read is several complete
    /// branches -- and the live-query noun cannot yet carry more than one.
    ///
    /// This is a REFUSAL, not a degraded answer: collapsing the branches into
    /// one `Pinned` set would return a confidently wrong cross-product, and
    /// dropping branches would silently under-resolve. The composite
    /// live-query shape is #1108's; this variant is deleted the moment it
    /// lands.
    MultiHostReadRequiresUnionQuery { hosts: usize },
}

impl From<GroupContextError> for GroupReadError {
    fn from(error: GroupContextError) -> Self {
        Self::Context(error)
    }
}

impl std::fmt::Display for GroupReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Context(error) => write!(f, "{error}"),
            Self::MultiHostReadRequiresUnionQuery { hosts } => write!(
                f,
                "a read over {hosts} group hosts is {hosts} independent demand branches, and \
                 one live query cannot yet carry more than one (#1108)"
            ),
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
    match branches.len() {
        1 => Ok(LiveQuery(branches.pop().expect("exactly one branch"))),
        hosts => Err(GroupReadError::MultiHostReadRequiresUnionQuery { hosts }),
    }
}
