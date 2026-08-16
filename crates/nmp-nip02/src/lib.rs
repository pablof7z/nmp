//! Pure NIP-02 vocabulary: the one reactive live demand for the current
//! account's kind:3 contact list. `nostr` + `nmp-grammar` only, no engine, no
//! mechanism.
//!
//! The follow/unfollow write door, the kind:3 materializer, and the
//! following observation all need the engine itself (`WriteIntent`,
//! `Row`, receipt custody, live-query folding), so they live one layer up
//! at `nmp::nip02` (#1143) rather than here -- the identical split
//! `nmp::nip29`/`nmp-nip29` already uses. This crate depends on nothing that
//! depends on it; `nmp -> nmp-nip02` is the only edge, never the reverse.

mod demand;

pub use demand::current_account_demand;
