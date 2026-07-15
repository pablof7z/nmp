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
//! atom's current covering session set, `store` for each `(atom, relay)`'s
//! proven interval, and the engine's own connection bookkeeping for each
//! session's current link status).

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::{AccessContext, ConcreteFilter, ContextualAtom, RelaySessionKey};
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
    /// One entry per SESSION (relay URL + frozen access context) that
    /// currently covers at least one atom in the query's subtree (interior
    /// `Derived` atoms included — #12). Sorted by session key (relay URL,
    /// then access) for deterministic equality: `refresh_handle`'s
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
    /// Frozen access identity of the physical session producing this fact.
    pub access: AccessContext,
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
/// connection bookkeeping `EngineCore` already keeps for reconnect/replay)
/// and — since #8's session-identity half — `AwaitingAuth` (a protected
/// session that is connected but whose exact current generation has not yet
/// completed AUTH); `AuthDenied` remains RESERVED for #8's AUTH wire half,
/// and `Error` is reserved until a transport-level error fold lands (#51) —
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
    /// #8: a PROTECTED (`AccessContext::Nip42`) session is connected, but
    /// its exact current connection generation has not yet completed AUTH —
    /// its planned REQs are parked, so it is honestly not `Requesting` yet.
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
    /// The one whole-demand relay ceiling removed at least one otherwise-
    /// routable source for this atom. The atom may still have partial
    /// `sources`; this fact prevents that subset from masquerading as the
    /// complete requested acquisition.
    LocalLimit { atom: ConcreteFilter },
}

/// Compute `subtree_atoms`' [`AcquisitionEvidence`] against `plan` (each
/// atom's current covering SESSION set — the sessions whose compiled
/// `WireReq` absorbs that atom's key), `store` (each `(atom, relay)`'s
/// proven interval), and the engine's own `connected`/`auth_ready`/
/// `ever_connected` session sets (for `status`). Replaces `query_coverage`'s
/// min-over-everything collapse: this never returns a single verdict, only
/// per-source facts plus explicit shortfall.
///
/// - `subtree_atoms` must include every atom in the query's FULL subtree
///   (`nmp_resolver::Engine::subtree_atoms`), not just its root atoms (#12)
///   — an interior `Derived`'s inner-filter atom's covering relay is
///   consulted exactly like a root atom's would be. Each atom carries its
///   own `source`/`access` (#106) so `coverage_key` can look up the
///   correctly-scoped row; `ShortfallFact`'s own `atom` field stays a bare
///   `ConcreteFilter` (unchanged public surface) since it's reporting
///   which SELECTION lacks a source, not a context distinction.
/// - An empty `subtree_atoms` as a WHOLE is `ShortfallFact::NoResolvedDemand`
///   (never a silently-empty `sources`/`shortfall` pair).
/// - A subtree atom with an EMPTY covering set contributes
///   `ShortfallFact::NoPlannedSource` and contributes to no source.
/// - For each session that covers at least one subtree atom:
///   `reconciled_through = Some(min over covered atoms' proven `through`)`
///   IFF every atom it covers has a proven row (`from <= window floor`),
///   else `None`. `status` is `Requesting` if the session is currently
///   connected AND (Public, or its exact current generation has completed
///   AUTH); `AwaitingAuth` if connected but protected and not yet ready;
///   `Disconnected` if it has connected before but is not connected now;
///   else `Connecting` (planned but never yet connected).
/// - Sources are returned sorted by session key for deterministic equality.
pub(crate) fn acquisition_evidence<S: EventStore>(
    subtree_atoms: &BTreeSet<ContextualAtom>,
    plan: &RelayPlan,
    store: &S,
    connected: &BTreeSet<RelaySessionKey>,
    auth_ready: &BTreeSet<RelaySessionKey>,
    ever_connected: &BTreeSet<RelaySessionKey>,
) -> AcquisitionEvidence {
    if subtree_atoms.is_empty() {
        return AcquisitionEvidence {
            sources: Vec::new(),
            shortfall: vec![ShortfallFact::NoResolvedDemand],
        };
    }

    // session -> (every covered atom proven so far?, min proven `through`).
    let mut per_session: BTreeMap<RelaySessionKey, (bool, Option<Timestamp>)> = BTreeMap::new();
    let mut shortfall = Vec::new();

    for atom in subtree_atoms {
        let key = coverage_key(atom);
        let locally_limited = plan.limited.contains(&key);
        let covering: Vec<&RelaySessionKey> = plan
            .reqs
            .iter()
            .filter_map(|(session, reqs)| {
                reqs.iter()
                    .any(|r| r.absorbed.contains(&key))
                    .then_some(session)
            })
            .collect();

        if locally_limited {
            shortfall.push(ShortfallFact::LocalLimit {
                atom: atom.filter.clone(),
            });
        }

        if covering.is_empty() {
            if !locally_limited {
                shortfall.push(ShortfallFact::NoPlannedSource {
                    atom: atom.filter.clone(),
                });
            }
            continue;
        }

        let window_start = Timestamp::from(atom.filter.since.unwrap_or(0));
        for session in covering {
            let entry = per_session.entry(session.clone()).or_insert((true, None));
            // Coverage rows stay keyed (context-hashed key, relay URL): the
            // access distinction already lives inside `key` itself
            // (`CoverageKey` is a context-inclusive hash), so the store read
            // needs only the session's relay.
            match store.get_coverage(key, &session.relay) {
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

    let sources = per_session
        .into_iter()
        .map(|(session, (all_proven, through))| {
            let status = if connected.contains(&session) {
                if session.access == AccessContext::Public || auth_ready.contains(&session) {
                    SourceStatus::Requesting
                } else {
                    // Connected but protected and its exact current
                    // generation has not completed AUTH: its planned REQs
                    // are parked, so `Requesting` would be a lie.
                    SourceStatus::AwaitingAuth {
                        phase: AuthPhase::AwaitingPolicy,
                    }
                }
            } else if ever_connected.contains(&session) {
                SourceStatus::Disconnected
            } else {
                SourceStatus::Connecting
            };
            SourceEvidence {
                relay: session.relay,
                access: session.access,
                reconciled_through: if all_proven { through } else { None },
                status,
            }
        })
        .collect();

    AcquisitionEvidence { sources, shortfall }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nmp_grammar::{AccessContext, SourceAuthority};
    use nmp_router::{SubId, WireReq};
    use nmp_store::MemoryStore;

    fn atom() -> ContextualAtom {
        ContextualAtom {
            filter: ConcreteFilter {
                kinds: Some(BTreeSet::from([1])),
                authors: Some(BTreeSet::from(["aa".repeat(32)])),
                ..ConcreteFilter::default()
            },
            source: SourceAuthority::AuthorOutboxes,
            access: AccessContext::Public,
            routing_evidence: BTreeSet::new(),
        }
    }

    #[test]
    fn a_partially_planned_atom_keeps_local_limit_evidence() {
        let atom = atom();
        let key = coverage_key(&atom);
        let relay = RelayUrl::parse("wss://relay.example").unwrap();
        let req = WireReq {
            sub_id: SubId::for_wire(relay.clone(), &atom.filter, &atom.source, atom.access),
            filter: atom.filter.clone(),
            provenance: Vec::new(),
            absorbed: BTreeSet::from([key]),
        };
        let plan = RelayPlan {
            reqs: BTreeMap::from([(RelaySessionKey::public(relay.clone()), vec![req])]),
            limited: BTreeSet::from([key]),
            refused_sessions: BTreeSet::from([RelaySessionKey::public(
                RelayUrl::parse("wss://refused.example").unwrap(),
            )]),
        };

        let evidence = acquisition_evidence(
            &BTreeSet::from([atom.clone()]),
            &plan,
            &MemoryStore::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
        );

        assert_eq!(evidence.sources.len(), 1);
        assert_eq!(evidence.sources[0].relay, relay);
        assert_eq!(
            evidence.shortfall,
            vec![ShortfallFact::LocalLimit { atom: atom.filter }]
        );
    }

    #[test]
    fn a_fully_refused_atom_reports_limit_not_no_source() {
        let atom = atom();
        let key = coverage_key(&atom);
        let plan = RelayPlan {
            limited: BTreeSet::from([key]),
            refused_sessions: BTreeSet::from([RelaySessionKey::public(
                RelayUrl::parse("wss://refused.example").unwrap(),
            )]),
            ..RelayPlan::default()
        };

        let evidence = acquisition_evidence(
            &BTreeSet::from([atom.clone()]),
            &plan,
            &MemoryStore::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
        );

        assert!(evidence.sources.is_empty());
        assert_eq!(
            evidence.shortfall,
            vec![ShortfallFact::LocalLimit { atom: atom.filter }]
        );
    }
}
