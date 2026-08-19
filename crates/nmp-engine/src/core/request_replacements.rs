//! Typed ownership for accepted-open-before-close request replacements
//! (#774): every successor `SubId` waiting to retire its predecessor once its
//! own admission settles, plus which relay session each pending transition
//! belongs to.
//!
//! ## The mirrored-index mechanism
//!
//! A forward map of children keyed by their own wire id (here, the successor
//! `SubId`), and a reverse index from the thing that owns them (here, a
//! `RelaySessionKey`, not a plan `SubId`). Before writing this file, the
//! insert/take/teardown rules were checked against `PlanIndexed`'s rather
//! than assumed identical from the shape alone: `insert` refuses a duplicate
//! child in both, and once the forward map says a child existed, `take` and
//! the owner-scoped teardown both require its reverse edge to exist rather
//! than tolerate it going missing. No rule differed. Only the owner's key
//! type does — `RelaySessionKey` here, plan `SubId` there — so the mechanism
//! moved to `owner_index.rs`, generic over that one type, and both owners use
//! it instead of each hand-rolling its own copy.
//!
//! ## The two silent-tolerance sites this closes
//!
//! Before this owner existed, `take_request_replacement` pruned the reverse
//! set behind `if let Some(successors) = ...`, silently accepting a forward
//! entry whose session had no reverse set at all. And
//! `abandon_request_replacements_for_session` looped removing successors from
//! the forward map with `else { continue }`, silently accepting a reverse
//! edge naming a successor the forward map had already lost. Both are now
//! [`OwnerIndexed::take`](super::owner_index::OwnerIndexed::take) and
//! [`OwnerIndexed::take_owner`](super::owner_index::OwnerIndexed::take_owner),
//! which panic on exactly that disagreement — the identical fix
//! `PlanIndexed::take` and `PlanIndexed::take_owner` already got.
//!
//! Fields are private. Every reach-in from `query.rs` and `auth_transport.rs`
//! is a named method on this owner instead of a raw map operation.

use nmp_grammar::RelaySessionKey;
use nmp_router::{RequestReplacement, SubId};

use super::owner_index::{IndexedChild, OwnerIndexed};

impl IndexedChild<RelaySessionKey> for RequestReplacement {
    fn owner_key(&self) -> &RelaySessionKey {
        &self.session
    }
}

pub(super) struct RequestReplacements {
    pending: OwnerIndexed<RelaySessionKey, SubId, RequestReplacement>,
}

impl Default for RequestReplacements {
    fn default() -> Self {
        Self {
            pending: OwnerIndexed::new("request replacement"),
        }
    }
}

impl RequestReplacements {
    /// Index one accepted-open-before-close transition under the session that
    /// owns it, keyed by its successor `SubId`.
    ///
    /// Duplicate successors are refused rather than replaced, the same rule
    /// `PlanIndexed::insert` enforces: a compile mints a fresh successor id
    /// per transition, so one successor owning two replacement records means
    /// the router double-planned one transition.
    pub(super) fn insert(&mut self, replacement: RequestReplacement) {
        let successor = replacement.next_sub_id.clone();
        self.pending.insert(successor, replacement);
    }

    /// Remove one pending transition by its successor, if it is still
    /// pending. Absent is a valid answer -- callers legitimately probe a
    /// successor that never became a replacement, or that already settled.
    pub(super) fn take(&mut self, successor: &SubId) -> Option<RequestReplacement> {
        self.pending.take(successor)
    }

    pub(super) fn contains(&self, successor: &SubId) -> bool {
        self.pending.contains(successor)
    }

    /// Remove every transition pending on one session, e.g. when its
    /// connection generation dies. The returned order is the reverse index's
    /// own, stable because its sets are ordered.
    pub(super) fn take_for_session(
        &mut self,
        session: &RelaySessionKey,
    ) -> Vec<(SubId, RequestReplacement)> {
        self.pending.take_owner(session)
    }

    #[cfg(any(test, feature = "test-instrumentation"))]
    pub(super) fn assert_consistent(&self, at: &str) {
        self.pending.assert_consistent(at);
    }

}
