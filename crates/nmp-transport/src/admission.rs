//! Transport-facing import of the one shared pure relay-host classifier.
//!
//! The classifier is a value-level operation in `nmp-grammar`; this crate
//! retains only transport's provenance-aware admission and resolved-address
//! connection behavior.

pub use nmp_grammar::relay::{
    classify_ip, classify_relay_host, normalize_bare_host, relay_host_key, RelayHostClass,
};
