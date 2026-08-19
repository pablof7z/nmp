//! NIP-11 relay information documents: the values, and the one-shot
//! acquisition service that produces them.
//!
//! NIP-11 is HTTP state, not a reactive stream, and that is why this is a
//! crate rather than a module inside `nmp`. Acquisition needs an HTTP client;
//! the reducer must not have one. The manifest is where that gets proved:
//! `reqwest` and `httpdate` are named by this package and by no other package
//! in the engine's tree, so an `EngineCore` build links no HTTP stack at all.
//!
//! What crosses back into the engine is a value, not a client. `nmp`'s runtime
//! projects a [`RelayInformationSnapshot`] into the reducer's own capability
//! vocabulary (`nmp::mechanism::core::RelayInformationCapabilityEvidence`) --
//! the same shape `nmp-runtime`'s loop uses to turn an author-route
//! provider's answer into an `AuthorRouteUpdate`. This crate deliberately
//! does not know that type exists.
//!
//! The service owns its state (a bounded cache, a per-relay single flight, an
//! active-fetch semaphore), its lifecycle ([`RelayInformationService::close`],
//! and a 3s overall fetch deadline that serves the engine's public sub-5s
//! shutdown contract), and its failure domain: the last good document is
//! retained separately from the last acquisition error, so a transient failure
//! never destroys useful presentation or capability evidence.

mod service;
mod value;

#[cfg(feature = "test-instrumentation")]
pub use service::RelayInformationRetentionCensus;
pub use service::RelayInformationService;
pub use value::{
    RelayInformationCachePolicy, RelayInformationDocument, RelayInformationError,
    RelayInformationFreshness, RelayInformationLimitations, RelayInformationSnapshot,
};
