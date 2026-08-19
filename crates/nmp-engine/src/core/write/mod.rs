//! Durable write, receipt, recovery, and retry lifecycle.
//!
//! This module owns acceptance through signing, route snapshots, per-relay
//! attempts and acknowledgements, cancellation/compensation, and boot recovery.

use nmp_grammar::RelaySessionKey;
use super::coordinate_coverage::CoordinateCoverage;
use super::*;
use nmp_grammar::ThreadPosition;
use nostr::nips::nip01::Coordinate;

// `pub(super)` so `core::cell` can name the call/continuation/outcome types
// in the checked doors it wraps. Still invisible outside `core`.
pub(super) mod replaceable_operation;
pub use replaceable_operation::{PreparedReplaceableMaterialization, PublishPreparation};
use replaceable_operation::{
    ReplaceableMaterializationCall, ReplaceableMaterializationOutcome, ReplaceableSuccessorInput,
};

fn public_retry_cause(cause: PublishQueueTransientCause) -> Option<RetryCause> {
    match cause {
        PublishQueueTransientCause::Interrupted => Some(RetryCause::Interrupted),
        PublishQueueTransientCause::AckTimeout => Some(RetryCause::AckTimeout),
        PublishQueueTransientCause::ConnectionLost => Some(RetryCause::ConnectionLost),
        PublishQueueTransientCause::RelayRateLimited => Some(RetryCause::RelayRateLimited),
        PublishQueueTransientCause::RelayError => Some(RetryCause::RelayError),
        PublishQueueTransientCause::AuthRequired => None,
    }
}

pub(in crate::core) fn public_auth_denial_source(
    source: StoredAuthDenialSource,
) -> AuthDenialSource {
    match source {
        StoredAuthDenialSource::Policy => AuthDenialSource::Policy,
        StoredAuthDenialSource::Signer => AuthDenialSource::Signer,
        StoredAuthDenialSource::Relay => AuthDenialSource::Relay,
    }
}

/// The frozen body as a signer sees it. Acceptance decided the author and
/// the timestamp, so this is the first point at which a complete unsigned
/// event exists at all — which is exactly why the payload an app hands in
/// is a builder and not one of these.
fn unsigned_from_frozen(frozen: &SignedEvent) -> UnsignedEvent {
    UnsignedEvent {
        id: Some(frozen.id),
        pubkey: frozen.pubkey,
        created_at: frozen.created_at,
        kind: frozen.kind,
        tags: frozen.tags.clone(),
        content: frozen.content.clone(),
    }
}

/// One execution of a routing strategy: what it can reach RIGHT NOW, what it
/// is still missing, and whether it can ever change its mind again.
///
/// `complete` is the retirement flag, and it is a statement about KNOWLEDGE
/// EXHAUSTION, never about delivery: an intent can be fully routed with every
/// lane undelivered, and can (transiently) be delivering on some lanes while
/// its routing is still incomplete
/// (`docs/internals/routing/resolution-lifecycle.md` §7.1). Nothing in this
/// struct is ever serialized — the journal stores the strategy label and the
/// committed relay revisions, never a resolution report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct RouteAnswer {
    /// Every relay this execution can name. Diffed against the intent's
    /// durable revision union by the caller, so re-running a resolution that
    /// learned nothing costs an empty diff and mints no lane.
    pub(super) relays: BTreeSet<RelayUrl>,
    /// The public keys whose neutral author-route provider must remain live.
    /// Usually these are `Unknown`; a zero-destination answer also retains
    /// its settled contributors because a later positive replacement is the
    /// only fact that can unpark it. These stateless declared needs are
    /// re-derived every pass and unioned across all parked writes.
    pub(super) author_route_needs: BTreeSet<PublicKey>,
    /// True iff nothing is left to learn, so re-executing is pointless and
    /// the `Auto` retires.
    ///
    /// A non-empty `author_route_needs` always forces this false — an answer
    /// still naming who it waits on is not settled — but the converse does
    /// not hold, and reading it as an equivalence is what would make an audit
    /// stop looking. Empty needs with `complete == false` is exactly how an
    /// answer says "settled, and I could not read the parent's canonical
    /// source this pass"; an `Explicit` route naming no relay says it too.
    /// What the pair does buy is that when needs ARE the reason,
    /// [`WriteFact::Destinations`] can carry that reason as keys rather than
    /// as a rendered sentence (#1236) — the sentence this struct used to
    /// carry was unbranchable by construction, so it is gone rather than
    /// reworded.
    pub(super) complete: bool,
}

mod coordinate_coverage;
mod write_lifecycle;
mod boot_recovery;
mod replaceable_succession;
mod publish_accept;
mod receipt_queue;
mod route_resolution;
