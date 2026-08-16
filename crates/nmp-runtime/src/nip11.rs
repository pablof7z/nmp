//! NIP-11 glue, beside the loop that drives it.
//!
//! Acquisition lives in `nmp-nip11`, which owns an HTTP client. The reducer
//! must not, so it consumes [`RelayInformationCapabilityEvidence`] — a value
//! `core` defines itself — and this module is the one place the two
//! vocabularies meet. It holds no `EngineCore`, issues no `EngineMsg`, and
//! produces no `Effect`; it is exactly the shape [`super::nip65`] uses for
//! `CoordinatorUpdate -> AuthorRouteUpdate`, for the same reason.
//!
//! Direction matters more than size here. Were this projection a method on
//! `RelayInformationSnapshot` back in `nmp-nip11`, that crate would have to
//! name the reducer's type, and `core` would end up reachable from the
//! package that links `reqwest`. Ten lines on this side of the line keep the
//! only edge pointing down: `runtime -> nmp-nip11`.

use nmp_nip11::RelayInformationSnapshot;

use nmp_engine::core::RelayInformationCapabilityEvidence;

/// The provenance-bearing subset of an acquired document that engine
/// capability decisions and diagnostics are allowed to see. Runtime
/// connection/AUTH state is deliberately not in it, and the acquisition
/// error arrives already rendered: the reducer's only use of it is the
/// diagnostics string.
pub(crate) fn capability_evidence(
    snapshot: &RelayInformationSnapshot,
) -> RelayInformationCapabilityEvidence {
    RelayInformationCapabilityEvidence {
        supported_nips: snapshot.document().supported_nips.clone(),
        max_subscriptions: snapshot.document().limitation.max_subscriptions,
        max_subid_length: snapshot.document().limitation.max_subid_length,
        document_revision: snapshot.document_revision().to_owned(),
        fresh_until: snapshot.fresh_until(),
        last_error: snapshot.last_error().map(ToString::to_string),
    }
}
