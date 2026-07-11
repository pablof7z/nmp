//! Per-query acquisition evidence
//! (`docs/design/scoped-evidence-49-12-plan.md` §2/§3/§4, folding #12 into
//! #49). Replaces the old `QueryCoverage::CompleteUpTo(T) | Unknown`
//! aggregate (the original ruling §6 sketch,
//! `docs/consults/2026-07-11-fable-coverage-attribution.md`): that type
//! collapsed every planned source's per-relay evidence into ONE
//! query-global verdict via a min-over-everything aggregation. #12 found
//! the resulting lie firsthand — a `Derived` query's OUTER atoms could all
//! be proven while an INNER atom (an interior `Derived`'s own inner filter)
//! was still entirely unproven, and the min-aggregate hid the interior atom
//! from the computation altogether (it only ever consulted
//! `Engine::root_atoms`). #49 requires the collapse itself deleted: no
//! public value may be named or read as global completeness,
//! authoritative-empty, synced, converged, or sync-health.
//!
//! This module produces per-SOURCE facts instead of a verdict: an app sees
//! WHICH relay has and hasn't proven WHAT for a query's full subtree, and
//! an explicit shortfall list for what nothing is even trying to acquire.
//! No aggregate/roll-up lives here or anywhere on this surface — that is
//! the app's own interpretation to make, never NMP's claim.
//!
//! No mutation, no attribution bookkeeping — that lives in `attribution.rs`;
//! this module only READS what has already been recorded (`plan` for each
//! atom's current covering relay set, `store` for each `(atom, relay)`'s
//! proven interval, and the engine's own connection bookkeeping for each
//! relay's current link status).

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::ConcreteFilter;
use nmp_router::RelayPlan;
use nmp_store::{coverage_key, EventStore};
use nostr::{RelayUrl, Timestamp};

/// Compact acquisition evidence for one query snapshot, scoped to THIS
/// query's own current subtree demand + plan — never engine-global, never
/// an authoritative claim. No field here is named or documented complete /
/// authoritative-empty / synced / converged / sync-health, and NOTHING on
/// this surface (Rust or otherwise) may add an `is_complete`/`is_settled`
/// style aggregate — an app rolls per-source facts into its own progress
/// policy; NMP never does that rollup for it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AcquisitionEvidence {
    /// One entry per relay that currently covers at least one atom in the
    /// query's subtree (interior `Derived` atoms included — #12). Sorted by
    /// relay URL for deterministic equality: `refresh_handle`'s
    /// change-detection compare must never spuriously fire on a mere
    /// re-ordering with no actual state change.
    pub sources: Vec<SourceEvidence>,
    /// Everything in the subtree with NO honest acquisition path today —
    /// the explicit, never-silent shortfall (the old "empty covering set /
    /// zero atoms ⇒ `Unknown`" branches, now local facts rather than a
    /// collapsed verdict). A query whose subtree yields zero atoms, or
    /// whose plan has zero covering relays for some of them, MUST surface
    /// here — never as a merely-empty `sources` list an app could misread
    /// as "nothing left to prove".
    pub shortfall: Vec<ShortfallFact>,
}

/// One relay's acquisition state for a query's subtree, as two DELIBERATELY
/// orthogonal facts: a durable PAST fact (`reconciled_through`, a persisted
/// watermark) and a current LINK fact (`status`). Keeping these independent
/// is load-bearing — a relay can be currently `Disconnected` while still
/// carrying a perfectly good `reconciled_through` from before it dropped
/// (the #49 acceptance criterion "offline cached rows remain usable"): if
/// the two were one enum, either the link state would shadow the watermark
/// or the watermark would shadow the link state, and either way the fact
/// that survives is a lie by omission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceEvidence {
    pub relay: RelayUrl,
    /// Durable per-(shape, relay) watermark evidence, min'd over the
    /// subtree atoms THIS source covers in THIS query, IFF every one of
    /// them has a coverage row whose `from` is at or before the query's own
    /// window floor. `None` = unproven (at least one covered atom has no
    /// such row yet) — never read as "complete", it is simply the absence
    /// of a fact. Independent of `status`.
    pub reconciled_through: Option<Timestamp>,
    /// Current link/acquisition status — orthogonal to `reconciled_through`.
    pub status: SourceStatus,
}

/// The closed, honest per-source link-status vocabulary. This frame
/// populates `Requesting`/`Connecting`/`Disconnected` (folded from the same
/// connection bookkeeping `EngineCore` already keeps for reconnect/replay);
/// `AwaitingAuth`/`AuthDenied` are RESERVED for #8's AUTH wire half, and
/// `Error` is reserved until a transport-level error fold lands (#51) —
/// every ratified variant is either populated here or documented reserved
/// with a named landing issue, never vocabulary nothing can ever emit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceStatus {
    /// The relay is connected and at least one subtree atom this source
    /// covers has an outstanding REQ — `reconciled_through` may still be
    /// `Some` from a prior window even while this reads `Requesting` (a
    /// live subscription's REQ does not close once its first EOSE lands).
    Requesting,
    /// A subtree atom this source covers has a planned wire request, but
    /// the relay has never yet completed a connection to deliver it.
    Connecting,
    /// The relay HAD connected at least once and is currently offline.
    /// `reconciled_through` (if `Some`) is the contract's own "cached-only"
    /// fact: rows already acquired through this source remain usable; this
    /// status alone never invalidates them.
    Disconnected,
    /// Reserved for #8 (AUTH evidence half) — NOT populated by this frame.
    AwaitingAuth { phase: AuthPhase },
    /// Reserved for #8 — terminal, NOT populated by this frame.
    AuthDenied,
    /// Reserved; populated by #51 (expand diagnostics error evidence) once
    /// transport surfaces a distinct per-relay wire-error state.
    Error,
}

/// #8: the AUTH negotiation phases worth surfacing while awaiting proof.
/// Deliberately excludes a completed/denied phase — an authenticated
/// source is just `Requesting`/carrying a `reconciled_through`, and
/// `AuthDenied` is its own top-level [`SourceStatus`], never a phase of
/// "awaiting" (an `AwaitingAuth` that could express "already authenticated"
/// or "already denied" would be a representable non-state).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthPhase {
    AwaitingPolicy,
    AwaitingSignature,
}

/// An explicit, never-silent shortfall in a query's subtree acquisition —
/// facts about what nothing is (yet) trying to acquire, never folded into
/// `sources`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShortfallFact {
    /// A subtree atom has NO relay in the current plan whose wire filter
    /// absorbs it — nothing is even trying to acquire it (formerly
    /// `NoCandidates`).
    NoPlannedSource { atom: ConcreteFilter },
    /// The query's subtree resolved to ZERO atoms at all (e.g. a `Derived`
    /// binding whose own resolved set is currently empty) — distinct from
    /// `NoPlannedSource`, which requires at least one atom to exist. Without
    /// this fact, a vacuous subtree would present as a trivially-empty
    /// `sources` list, which an app could misread as "nothing left to
    /// prove" rather than "nothing has been asked for yet".
    NoResolvedDemand,
    /// An explicit local cap prevented full acquisition. Reserved: no path
    /// on this frame constructs it yet (no local-limit source exists on
    /// the read path today).
    LocalLimit { atom: ConcreteFilter },
}

/// Compute `subtree_atoms`' [`AcquisitionEvidence`] against `plan` (each
/// atom's current covering relay set — the relays whose compiled `WireReq`
/// absorbs that atom's key), `store` (each `(atom, relay)`'s proven
/// interval), and the engine's own `connected`/`ever_connected` relay sets
/// (for `status`). Replaces `query_coverage`'s min-over-everything collapse:
/// this never returns a single verdict, only per-source facts plus explicit
/// shortfall.
///
/// - `subtree_atoms` must include every atom in the query's FULL subtree
///   (`nmp_resolver::Engine::subtree_atoms`), not just its root atoms (#12)
///   — an interior `Derived`'s inner-filter atom's covering relay is
///   consulted exactly like a root atom's would be.
/// - An empty `subtree_atoms` as a WHOLE is `ShortfallFact::NoResolvedDemand`
///   (never a silently-empty `sources`/`shortfall` pair).
/// - A subtree atom with an EMPTY covering set contributes
///   `ShortfallFact::NoPlannedSource` and contributes to no source.
/// - For each relay that covers at least one subtree atom:
///   `reconciled_through = Some(min over covered atoms' proven `through`)`
///   IFF every atom it covers has a proven row (`from <= window floor`),
///   else `None`. `status` is `Requesting` if the relay is currently
///   connected, `Disconnected` if it has connected before but is not
///   connected now, else `Connecting` (planned but never yet connected).
/// - Sources are returned sorted by relay URL for deterministic equality.
pub(crate) fn acquisition_evidence<S: EventStore>(
    subtree_atoms: &BTreeSet<ConcreteFilter>,
    plan: &RelayPlan,
    store: &S,
    connected: &BTreeSet<RelayUrl>,
    ever_connected: &BTreeSet<RelayUrl>,
) -> AcquisitionEvidence {
    if subtree_atoms.is_empty() {
        return AcquisitionEvidence {
            sources: Vec::new(),
            shortfall: vec![ShortfallFact::NoResolvedDemand],
        };
    }

    // relay -> (every covered atom proven so far?, min proven `through`).
    let mut per_relay: BTreeMap<RelayUrl, (bool, Option<Timestamp>)> = BTreeMap::new();
    let mut shortfall = Vec::new();

    for atom in subtree_atoms {
        let key = coverage_key(atom);
        let covering: Vec<&RelayUrl> = plan
            .reqs
            .iter()
            .filter_map(|(relay, reqs)| {
                reqs.iter()
                    .any(|r| r.absorbed.contains(&key))
                    .then_some(relay)
            })
            .collect();

        if covering.is_empty() {
            shortfall.push(ShortfallFact::NoPlannedSource { atom: atom.clone() });
            continue;
        }

        let window_start = Timestamp::from(atom.since.unwrap_or(0));
        for relay in covering {
            let entry = per_relay.entry(relay.clone()).or_insert((true, None));
            match store.get_coverage(key, relay) {
                Some(interval) if interval.from <= window_start => {
                    entry.1 = Some(match entry.1 {
                        None => interval.through,
                        Some(cur) => cur.min(interval.through),
                    });
                }
                _ => entry.0 = false,
            }
        }
    }

    let sources = per_relay
        .into_iter()
        .map(|(relay, (all_proven, through))| {
            let status = if connected.contains(&relay) {
                SourceStatus::Requesting
            } else if ever_connected.contains(&relay) {
                SourceStatus::Disconnected
            } else {
                SourceStatus::Connecting
            };
            SourceEvidence {
                relay,
                reconciled_through: if all_proven { through } else { None },
                status,
            }
        })
        .collect();

    AcquisitionEvidence { sources, shortfall }
}
