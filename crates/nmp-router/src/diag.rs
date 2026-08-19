//! [`Diagnostics`] — the acceptance-test-made-visible, read-only projection
//! of a compiled plan (M2 plan §2.6): per-relay sub counts, lane counts,
//! reverse coverage (authors served), the exact filters sent, uncovered
//! authors, dropped merge rules, and what each relay advertised about its own
//! limits.

use std::collections::BTreeMap;

use nmp_grammar::{ConcreteFilter, RelaySessionKey};

use crate::facts::{Lane, PublicKey};
use crate::solver::Shortfall;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayDiagnostics {
    pub session: RelaySessionKey,
    /// Concurrent subscriptions currently open on this session. THE durable
    /// contract of the subscription programme (#931): without a per-session
    /// count in diagnostics and in the acceptance suite, the next axis that
    /// escapes coalescing regresses silently and the whole exercise repeats.
    pub wire_sub_count: usize,
    pub by_lane: BTreeMap<Lane, usize>,
    /// Reverse coverage: distinct authors this relay covers.
    pub authors_served: usize,
    /// The EXACT filters sent to this relay.
    pub filters: Vec<ConcreteFilter>,
    /// What this relay advertised as `limitation.max_subscriptions`. `None`
    /// means it advertised nothing and is therefore UNBUDGETED — a
    /// distinction this never collapses into a fabricated number.
    pub subscription_budget: Option<usize>,
    /// Subscriptions this compile removed to stay inside
    /// `subscription_budget`. Every one of them is also reported as
    /// `limited` coverage, so the demand is refused visibly, never silently.
    pub subscriptions_refused: usize,
    /// What this relay advertised as `limitation.max_subid_length`.
    pub subid_length_limit: Option<usize>,
    /// True iff that advertised length is SHORTER than the 64-character ids
    /// NMP sends, i.e. this relay rejects every REQ we put on its socket.
    /// Diagnostic only — nothing here may ever reach id derivation.
    pub subid_length_rejects_our_ids: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Diagnostics {
    pub per_session: BTreeMap<RelaySessionKey, RelayDiagnostics>,
    pub uncovered_authors: BTreeMap<PublicKey, Shortfall>,
    /// Distinct candidates rejected by the one whole-demand relay ceiling.
    /// They are absent from `per_session` by construction.
    pub sessions_refused_by_cap: usize,
    /// Distinct sessions refused OUTRIGHT by a relay advertising zero
    /// concurrent subscriptions. Counted apart from the relay ceiling
    /// because the two answer different questions — "the operator's plan was
    /// too wide" versus "this relay will hold nothing open" — and a reader
    /// that conflated them could not tell which bound to relax. Also absent
    /// from `per_session`; a session merely TRIMMED by its budget is present
    /// there with a non-zero `subscriptions_refused`.
    pub sessions_refused_by_subscription_budget: usize,
    pub dropped_merge_rules: Vec<&'static str>,
}

