//! Per-query acquisition evidence
//! (`docs/design/scoped-evidence-49-12-plan.md` §2/§3/§4, folding #12 into
//! #49). Replaces the old `QueryCoverage::CompleteUpTo(T) | Unknown`
//! aggregate (the original ruling §6 sketch,
//! `docs/design/query-demand-and-evidence.md`): that type
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
use nmp_router::{RelayPlan, SubId};
use nmp_store::{coverage_key, EventStore, PersistenceError};
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
    /// then access) for deterministic equality: `refresh_observation`'s
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
/// watermark) and a fact about the request on the wire RIGHT NOW (`status`).
/// Keeping these independent is load-bearing — a relay can be currently
/// `Disconnected` while still carrying a perfectly good `reconciled_through`
/// from before it dropped (the #49 acceptance criterion "offline cached rows
/// remain usable"): if the two were one enum, either the link state would
/// shadow the watermark or the watermark would shadow the link state, and
/// either way the fact that survives is a lie by omission.
///
/// That axis, not the presence of a watermark, is where
/// [`SourceStatus::FinishedStoredEvents`] belongs (#1235). Whether this relay
/// has finished answering is current and dies with the socket, exactly like
/// every other status; whether it PROVED anything over the query's window is
/// durable and outlives it. The two are independent in both directions — a
/// router-bounded request finishes with no watermark, and a watermark from a
/// prior window is `Some` while a fresh request is still streaming.
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
    /// This session's current link and request state — orthogonal to
    /// `reconciled_through`, and like a socket rather than like a watermark:
    /// it describes right now, and nothing here survives the connection.
    pub status: SourceStatus,
}

/// The closed, honest per-source vocabulary for one session's current link
/// and request state. This frame populates every state below from exact
/// session/generation bookkeeping. AUTH policy/signer failures are
/// session-local facts; #51 may later add richer transport error detail
/// without changing this closed status shape.
///
/// Every member is ONE SOURCE's own fact. Nothing here reads across sources,
/// and no member may ever be added that does — that is the boundary this
/// closed shape protects, not the count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceStatus {
    /// The relay is connected and at least one subtree atom this source
    /// covers is still having its stored events streamed — `reconciled_through`
    /// may still be `Some` from a prior window even while this reads
    /// `Requesting`, because a watermark is a durable PAST fact and this is a
    /// fact about the request on the wire right now.
    Requesting,
    /// The relay is connected and every wire request covering this query's
    /// subtree atoms on this session has reached NIP-01's end of stored
    /// events — the relay has sent everything it had for the question it was
    /// asked (#1235).
    ///
    /// This is a DELIVERY fact about one relay answering one request, and it
    /// is deliberately none of the things this module refuses to say: it does
    /// not claim the query is complete, does not claim any other source has
    /// finished, and does not claim what this relay held. What the relay
    /// PROVED over the query's window is `reconciled_through` and only that —
    /// a request the router bounded with a NIP-01 `limit` finishes here while
    /// earning no watermark at all, which is exactly the pair
    /// `features/coverage/empty-vs-unknown.feature` distinguishes.
    ///
    /// Scoped to the request currently on the wire, like every other variant
    /// here and unlike `reconciled_through`: a session that finishes and then
    /// drops reads `Disconnected`, because replay opens a fresh request there
    /// is nothing yet to have finished. The live subscription itself stays
    /// open and keeps delivering new events; only its stored-events phase is
    /// over.
    FinishedStoredEvents,
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
    AwaitingAuth {
        phase: AuthPhase,
    },
    AuthDenied,
    /// The exact session's current AUTH policy/signer/send operation failed.
    /// This is not an aggregate relay-health judgment.
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
    AwaitingChallenge,
    AwaitingPolicy,
    AwaitingSignature,
    AwaitingRelayAck,
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
/// proven interval), and the engine's own `connected`/`auth_status`/
/// `ever_connected` session bookkeeping (for `status`). Replaces
/// `query_coverage`'s min-over-everything collapse: this never returns a
/// single verdict, only per-source facts plus explicit shortfall.
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
///   else `None`. `status` for a connected Public session is `Requesting`;
///   a connected PROTECTED session reads the AUTH reducer's own per-session
///   truth from `auth_status` (exact phase, `AuthDenied`, `Error`, or
///   `Requesting` once ready), defaulting to `AwaitingAuth {
///   AwaitingChallenge }` when the reducer holds no entry (connected but
///   never challenged); `Disconnected` if it has connected before but is
///   not connected now; else `Connecting` (planned but never yet
///   connected). A session that would read `Requesting` reads
///   [`SourceStatus::FinishedStoredEvents`] instead once EVERY wire request
///   absorbing an atom it covers appears in `finished_stored_events` — the
///   same all-or-nothing scoping `reconciled_through` uses, so one unfinished
///   request on a session is never hidden by a finished sibling (#1235).
/// - Sources are returned sorted by session key for deterministic equality.
pub(crate) fn acquisition_evidence<S: EventStore>(
    subtree_atoms: &BTreeSet<ContextualAtom>,
    plan: &RelayPlan,
    store: &S,
    connected: &BTreeSet<RelaySessionKey>,
    auth_status: &BTreeMap<RelaySessionKey, SourceStatus>,
    ever_connected: &BTreeSet<RelaySessionKey>,
    finished_stored_events: &BTreeSet<(RelaySessionKey, SubId)>,
) -> Result<AcquisitionEvidence, PersistenceError> {
    if subtree_atoms.is_empty() {
        return Ok(AcquisitionEvidence {
            sources: Vec::new(),
            shortfall: vec![ShortfallFact::NoResolvedDemand],
        });
    }

    // session -> (every covered atom proven so far?, min proven `through`,
    // every wire request covering an atom so far finished its stored events?).
    let mut per_session: BTreeMap<RelaySessionKey, (bool, Option<Timestamp>, bool)> =
        BTreeMap::new();
    let mut shortfall = Vec::new();

    for atom in subtree_atoms {
        let key = coverage_key(atom);
        let locally_limited = plan.limited.contains(&key);
        // Per covering SESSION, whether every one of ITS wire requests that
        // absorbs this atom has reached end of stored events. A session may
        // absorb one atom through several requests; the atom is only finished
        // on that session when all of them are.
        let covering: Vec<(&RelaySessionKey, bool)> = plan
            .reqs
            .iter()
            .filter_map(|(session, reqs)| {
                let mut absorbing = reqs.iter().filter(|r| r.absorbed.contains(&key)).peekable();
                absorbing.peek()?;
                let finished = absorbing
                    .all(|r| finished_stored_events.contains(&(session.clone(), r.sub_id.clone())));
                Some((session, finished))
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
        for (session, finished) in covering {
            let entry = per_session
                .entry(session.clone())
                .or_insert((true, None, true));
            entry.2 &= finished;
            // Coverage rows stay keyed (context-hashed key, relay URL): the
            // access distinction already lives inside `key` itself
            // (`CoverageKey` is a context-inclusive hash), so the store read
            // needs only the session's relay.
            // `?`, never a `_ =>` arm folding the error in with the misses
            // (#763). `reconciled_through: None` is this function telling an
            // app "this source has proven nothing over your window", and a
            // store that could not be read has not established that.
            match store.get_coverage(key, &session.relay)? {
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
        .map(|(session, (all_proven, through, all_finished))| {
            let status = if connected.contains(&session) {
                if session.access == AccessContext::Public {
                    SourceStatus::Requesting
                } else {
                    auth_status
                        .get(&session)
                        .copied()
                        .unwrap_or(SourceStatus::AwaitingAuth {
                            phase: AuthPhase::AwaitingChallenge,
                        })
                }
            } else if ever_connected.contains(&session) {
                SourceStatus::Disconnected
            } else {
                SourceStatus::Connecting
            };
            // Only the state that MEANS "stored events are still streaming"
            // can be displaced by their end. A session still negotiating AUTH,
            // denied, errored, disconnected, or never yet connected has no
            // request on the wire to have finished, so its own fact stands.
            let status = match (status, all_finished) {
                (SourceStatus::Requesting, true) => SourceStatus::FinishedStoredEvents,
                (status, _) => status,
            };
            SourceEvidence {
                relay: session.relay,
                access: session.access,
                reconciled_through: if all_proven { through } else { None },
                status,
            }
        })
        .collect();

    Ok(AcquisitionEvidence { sources, shortfall })
}

/// Combine independently-planned Demand scopes into one query snapshot
/// without allowing one scope's plan to prove another scope. The public
/// evidence shape remains per physical source, so a source appearing in
/// multiple scopes is folded to its least-proven watermark while explicit
/// shortfalls are unioned.
///
/// [`SourceStatus::FinishedStoredEvents`] folds the same way and for the same
/// reason (#1235): each scope plans its own wire requests, so one physical
/// session can have finished one scope's request while another scope's is
/// still streaming. Reporting the finish would let one scope's completion
/// answer for a scope nothing has answered — the exact borrowing this merge
/// exists to prevent. Every other status is a link fact, and one physical
/// session has one link status.
/// The LINK half of a status, with the request phase collapsed away. Two
/// scopes must agree about the socket; they may legitimately disagree about
/// whether the request each of them planned has finished.
fn link_status(status: SourceStatus) -> SourceStatus {
    match status {
        SourceStatus::FinishedStoredEvents => SourceStatus::Requesting,
        other => other,
    }
}

pub(crate) fn merge_acquisition_evidence(
    parts: impl IntoIterator<Item = AcquisitionEvidence>,
) -> AcquisitionEvidence {
    let mut sources: BTreeMap<(RelayUrl, AccessContext), SourceEvidence> = BTreeMap::new();
    let mut shortfall = Vec::new();

    for part in parts {
        for source in part.sources {
            let key = (source.relay.clone(), source.access);
            match sources.entry(key) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(source);
                }
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    let current = entry.get_mut();
                    debug_assert!(
                        link_status(current.status) == link_status(source.status),
                        "one physical session has one current link status"
                    );
                    if current.status == SourceStatus::FinishedStoredEvents
                        && source.status == SourceStatus::Requesting
                    {
                        current.status = SourceStatus::Requesting;
                    }
                    current.reconciled_through =
                        match (current.reconciled_through, source.reconciled_through) {
                            (Some(left), Some(right)) => Some(left.min(right)),
                            _ => None,
                        };
                }
            }
        }
        for fact in part.shortfall {
            if !shortfall.contains(&fact) {
                shortfall.push(fact);
            }
        }
    }

    AcquisitionEvidence {
        sources: sources.into_values().collect(),
        shortfall,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nmp_grammar::{AccessContext, SourceAuthority};
    use nmp_router::{SubId, WireReq};
    use nmp_store::MemoryStore;
    use nostr::Keys;

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
            source: atom.source.clone(),
            provenance: Vec::new(),
            absorbed: BTreeSet::from([key]),
        };
        let plan = RelayPlan {
            reqs: BTreeMap::from([(RelaySessionKey::public(relay.clone()), vec![req])]),
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
            &BTreeMap::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
        )
        .expect("MemoryStore coverage never fails");

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
            &BTreeMap::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
        )
        .expect("MemoryStore coverage never fails");

        assert!(evidence.sources.is_empty());
        assert_eq!(
            evidence.shortfall,
            vec![ShortfallFact::LocalLimit { atom: atom.filter }]
        );
    }

    #[test]
    fn protected_source_reports_each_exact_auth_phase_and_terminal_truth() {
        let mut atom = atom();
        atom.access = AccessContext::Nip42(Keys::generate().public_key());
        let relay = RelayUrl::parse("wss://protected-evidence.example").unwrap();
        let session = RelaySessionKey::new(relay.clone(), atom.access);
        let key = coverage_key(&atom);
        let plan = RelayPlan {
            reqs: BTreeMap::from([(
                session.clone(),
                vec![WireReq {
                    sub_id: SubId::for_wire(relay, &atom.filter, &atom.source, atom.access),
                    filter: atom.filter.clone(),
                    source: atom.source.clone(),
                    provenance: Vec::new(),
                    absorbed: BTreeSet::from([key]),
                }],
            )]),
            ..RelayPlan::default()
        };
        let connected = BTreeSet::from([session.clone()]);
        let cases = [
            SourceStatus::AwaitingAuth {
                phase: AuthPhase::AwaitingPolicy,
            },
            SourceStatus::AwaitingAuth {
                phase: AuthPhase::AwaitingSignature,
            },
            SourceStatus::AwaitingAuth {
                phase: AuthPhase::AwaitingRelayAck,
            },
            SourceStatus::AuthDenied,
            SourceStatus::Error,
            SourceStatus::Requesting,
        ];
        for status in cases {
            let evidence = acquisition_evidence(
                &BTreeSet::from([atom.clone()]),
                &plan,
                &MemoryStore::new(),
                &connected,
                &BTreeMap::from([(session.clone(), status)]),
                &connected,
                &BTreeSet::new(),
            )
            .expect("MemoryStore coverage never fails");
            assert_eq!(evidence.sources[0].status, status);
        }

        let waiting = acquisition_evidence(
            &BTreeSet::from([atom]),
            &plan,
            &MemoryStore::new(),
            &connected,
            &BTreeMap::new(),
            &connected,
            &BTreeSet::new(),
        )
        .expect("MemoryStore coverage never fails");
        assert_eq!(
            waiting.sources[0].status,
            SourceStatus::AwaitingAuth {
                phase: AuthPhase::AwaitingChallenge
            }
        );
    }
}
