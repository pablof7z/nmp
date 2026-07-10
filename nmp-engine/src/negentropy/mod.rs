//! Prober FSM + `ProbedRelay` capability token (plan §3.4, §1 "module not a
//! crate": reducer-coupled, driven turn-by-turn by `EngineCore`, sharing
//! its message vocabulary). HARVEST target:
//! `crates/nmp-nip77/src/{runtime,reconciler,filter,messages,codec}.rs` in
//! the old repo — the `RelayNegentropyState` FSM (-> [`ProbeState`] here),
//! `EligibleFilter` parse, `Reconciler` over the `negentropy` crate, and
//! the 30s liveness-deadline REQ fallback are re-justified there (plan
//! §4). The kernel "substrate seam" (outbound-REQ/inbound-text
//! interceptor) is DROPPED — E's reducer drives the prober directly.
//!
//! **Naming gotcha for E:** this module is named `negentropy`, the same
//! name as the external `negentropy` crate this workspace depends on
//! (`nmp-engine/Cargo.toml`). A local `mod negentropy` shadows the
//! extern-prelude name at the crate root, so bare `negentropy::Foo` inside
//! this crate resolves to THIS module, not the external crate. Refer to
//! the external crate as `::negentropy::Foo` (leading `::`) wherever E
//! wraps its `Reconciler`.
//!
//! Step 0 declares [`ProbeState`], [`ProbedRelay`], and [`Prober`] only —
//! no FSM transitions, no `Reconciler` wrapper yet (E, depends on B + A2).

use std::collections::HashMap;

use nostr::RelayUrl;

/// = the old repo's `RelayNegentropyState`, re-cut for the new reducer
/// vocabulary.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ProbeState {
    Unknown,
    Probing,
    Supported,
    Unsupported,
}

/// Capability TOKEN (ledger #8): constructible ONLY from a `Supported`
/// cache entry. `NegOpen`/negentropy-sync effects take a `ProbedRelay`,
/// never a bare `RelayUrl` — an unprobed relay cannot reach the negentropy
/// path; it gets a plain REQ instead.
///
/// The inner field is `pub(crate)` and Step 0 provides NO constructor
/// (deliberately — see plan §6, "E" depends on B + A2): the whole point of
/// this type is that only E's `Supported`-state code can ever mint one.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbedRelay(pub(crate) RelayUrl);

/// Per-relay negentropy-support cache the prober FSM maintains.
pub struct Prober {
    pub states: HashMap<RelayUrl, ProbeState>,
}
