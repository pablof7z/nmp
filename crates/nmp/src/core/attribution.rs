//! Coverage-attribution state for the request-scoped facts-before-claims
//! contract recorded in issue #816 and
//! `docs/design/query-demand-and-evidence.md`: the per-`SubId` FIFO of
//! send-time snapshots, the wire subscription-id -> `SubId` reverse lookup,
//! and the `CoverageKey` -> retained window-erased shape registry
//! `record_coverage` needs (the store only ever sees whatever
//! `ConcreteFilter` it is handed; `EngineCore` is the one place that knows
//! which shape a given key came from — see `nmp-store`'s own `ShapeRecord`
//! doc comment for the identical reasoning at the store layer).
//!
//! This is a plain data structure with no I/O and no access to the store or
//! router — `EngineCore` (`core/mod.rs`) is the one place both exist
//! together, and it is the one that actually calls `EventStore::
//! record_coverage` with the shape this module hands back.

use std::collections::{BTreeSet, HashMap, VecDeque};

use nmp_grammar::{ConcreteFilter, ContextualAtom, RelaySessionKey};
use nmp_router::SubId;
use nmp_store::{coverage_key, CoverageInterval, CoverageKey};
use nostr::Timestamp;

/// One send-time snapshot (ruling §2): what a single outgoing REQ (or NEG
/// session) proves, captured at the moment it was sent — never re-derived
/// from the sub's CURRENT filter later.
#[derive(Debug, Clone)]
struct AttributionSnapshot {
    send_id: AttributionSendId,
    event_failure_target: AttributionSendId,
    absorbed: BTreeSet<CoverageKey>,
    floor: Option<Timestamp>,
    until: Option<Timestamp>,
    coverage_authority: CoverageAuthority,
}

/// Opaque identity of one exact send-time attribution snapshot. Ordinary
/// EOSE is intentionally ambiguous when a subscription id is overwritten,
/// so it uses FIFO intersection. A NEG completion is correlated to the
/// exact NEG session that completed and uses this identity instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct AttributionSendId(u64);

impl AttributionSendId {
    pub(crate) fn revision(self) -> u64 {
        self.0
    }
}

/// Which request loses coverage authority if an EVENT delivered under the
/// send being recorded fails its event-store transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EventFailureTarget {
    /// Ordinary REQ, backlog REQ, live REQ, and NEG own their own failure.
    ThisSend,
    /// A temporary missing-id REQ is only the ingestion tail of the original
    /// NEG request, so its EVENT failures poison that retained owner.
    Correlated(AttributionSendId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoverageAuthority {
    Eligible,
    Poisoned(CoveragePoison),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CoveragePoison {
    LimitedRequest,
    EventCommitFailed,
    CoverageCommitFailed,
    MissingShape,
}

impl CoverageAuthority {
    fn poison(&mut self, reason: CoveragePoison) {
        if matches!(self, Self::Eligible) {
            *self = Self::Poisoned(reason);
        }
    }
}

/// One removed attribution owner. Completion owns both the exact request
/// identity and its monotonic coverage authority until the one persistence
/// door either commits every claim or retires it without a claim.
#[derive(Debug)]
pub(crate) struct CompletedAttribution {
    send_id: AttributionSendId,
    coverage_authority: CoverageAuthority,
    claims: Vec<(CoverageKey, CoverageInterval)>,
}

impl CompletedAttribution {
    pub(crate) fn send_id(&self) -> AttributionSendId {
        self.send_id
    }

    pub(crate) fn eligible_claims(&self) -> Option<&[(CoverageKey, CoverageInterval)]> {
        matches!(self.coverage_authority, CoverageAuthority::Eligible)
            .then_some(self.claims.as_slice())
    }

    pub(crate) fn poison(&mut self, reason: CoveragePoison) {
        self.coverage_authority.poison(reason);
    }
}

/// All coverage-attribution bookkeeping `EngineCore` owns. Keyed by `SubId`
/// (which already embeds the relay — `SubId(RelayUrl, SkeletonHash, AccessContext)`), so a
/// FIFO lookup is also implicitly relay-scoped.
#[derive(Debug, Default)]
pub(crate) struct AttributionState {
    next_send_id: u64,
    inflight: HashMap<SubId, VecDeque<AttributionSnapshot>>,
    /// `(session, wire-format subscription_id string) -> SubId`, populated at
    /// send time. `nmp-transport::Pool` is an unimplemented Step 0 shell in
    /// M3 step B, so there is no pre-existing wire convention to conform
    /// to; `EngineCore` invents and owns this string entirely (see
    /// `wire_sub_id_string` below) and is the only reader of it, via this
    /// map — never by re-parsing the string back into a hash. Keyed by the
    /// full [`RelaySessionKey`] (never a bare URL): NIP-42 visibility is
    /// connection-scoped, so the SAME wire string on the SAME relay under a
    /// DIFFERENT access context is a different physical session's sub and
    /// must never resolve to this one.
    sub_id_by_wire: HashMap<(RelaySessionKey, String), SubId>,
    /// `CoverageKey -> the ContextualAtom it came from (#106 -- widened
    /// from a bare `ConcreteFilter`: the store's `record_coverage` now
    /// takes a `&ContextualAtom`, since `CoverageKey` itself is a
    /// context-inclusive hash, so retaining only the selection shape would
    /// no longer be enough to reconstruct the right key at EOSE time).
    /// `EngineCore` only ever has a `CoverageKey` at attribution time (from
    /// `WireReq::absorbed`), so it must retain the FULL atom separately to
    /// be able to call that door at all. Pruned each recompile by
    /// [`Self::prune_shapes`] (mirroring `EngineCore`'s own
    /// `nip11_information` pruning in `core/mod.rs::recompile`) against the
    /// union of the current `active_demand()` and every `CoverageKey` still
    /// `absorbed` by an outstanding `inflight` snapshot — see that method's
    /// doc for why both sets are required.
    shape_by_key: HashMap<CoverageKey, ContextualAtom>,
}

impl AttributionState {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Learn the ContextualAtom behind every atom in `demand` (called once
    /// per recompile, from the resolver's full current `active_demand()` —
    /// cheap, and the only way `EngineCore` ever sees the atoms' shapes at
    /// all). `demand` carries each atom's full `ContextualAtom` identity
    /// (#106) so the retained value is keyed AND populated the SAME way
    /// `record_send`/`attribute_eose`'s `CoverageKey`s already are.
    pub(crate) fn observe_demand<'a>(
        &mut self,
        demand: impl IntoIterator<Item = &'a ContextualAtom>,
    ) {
        for atom in demand {
            self.shape_by_key
                .entry(coverage_key(atom))
                .or_insert_with(|| atom.clone());
        }
    }

    /// The retained atom for `key`, if any atom carrying it has ever been
    /// observed via [`Self::observe_demand`].
    pub(crate) fn shape_of(&self, key: CoverageKey) -> Option<ContextualAtom> {
        self.shape_by_key.get(&key).cloned()
    }

    /// Prune `shape_by_key` down to keys still reachable from SOMEWHERE
    /// (finding E3, epic #507): called once per recompile, right after
    /// [`Self::observe_demand`] (same `demand` argument), mirroring
    /// `EngineCore`'s own `nip11_information.retain(..)` immediately below
    /// it in `core/mod.rs::recompile` -- without this, `shape_by_key` grows
    /// once per distinct atom shape ever demanded for the life of the
    /// process, which for a long-running client visiting many distinct
    /// profiles/queries over a session is unbounded.
    ///
    /// A key is still reachable, and MUST be retained, if EITHER:
    /// - it is `coverage_key(atom)` for some atom in the CURRENT `demand`
    ///   (the same set [`Self::observe_demand`] was just called with), or
    /// - it is still `absorbed` by some snapshot outstanding in `inflight`.
    ///
    /// The second clause is load-bearing, not defensive: `attribute_eose`
    /// intersects EVERY still-outstanding snapshot on a sub (ruling §2,
    /// see its own doc), and a sub's outstanding snapshots can span
    /// multiple recompiles -- an atom can leave `active_demand()` (the
    /// resolver moves on) while its already-sent REQ is still awaiting
    /// EOSE, and that REQ's `absorbed` keys must keep resolving via
    /// `shape_of` whenever that EOSE (or NEG-MSG completion) finally
    /// arrives, arbitrarily many recompiles later. Pruning against
    /// `demand` alone would silently turn that later `shape_of` lookup
    /// into `None` and drop a coverage credit that was legitimately
    /// earned -- over-pruning, a correctness bug. Retaining a key that
    /// satisfies neither clause is merely a stale entry: still harmless,
    /// per this struct's own doc, exactly as it was before this method
    /// existed -- so under-pruning here is the acceptable failure mode,
    /// never over-pruning.
    pub(crate) fn prune_shapes<'a>(
        &mut self,
        demand: impl IntoIterator<Item = &'a ContextualAtom>,
    ) {
        let mut live: BTreeSet<CoverageKey> = demand.into_iter().map(coverage_key).collect();
        for fifo in self.inflight.values() {
            live.extend(fifo.iter().flat_map(|snap| snap.absorbed.iter().copied()));
        }
        self.shape_by_key.retain(|key, _| live.contains(key));
    }

    /// Record a send-time snapshot for a REQ just placed on the wire for
    /// `sub_id` on `session`, whose (possibly coalesced) filter is `filter`
    /// and which absorbs `absorbed` narrow atoms (from the `WireReq` this
    /// REQ was materialized from — ruling §2's containment rule, already
    /// discharged by `nmp-router::coalesce`).
    pub(crate) fn record_send(
        &mut self,
        session: &RelaySessionKey,
        sub_id: &SubId,
        filter: &ConcreteFilter,
        absorbed: BTreeSet<CoverageKey>,
        event_failure_target: EventFailureTarget,
    ) -> AttributionSendId {
        let send_id = AttributionSendId(self.next_send_id);
        self.next_send_id = self.next_send_id.wrapping_add(1);
        let snapshot = AttributionSnapshot {
            send_id,
            event_failure_target: match event_failure_target {
                EventFailureTarget::ThisSend => send_id,
                EventFailureTarget::Correlated(send_id) => send_id,
            },
            absorbed,
            floor: filter.since.map(Timestamp::from),
            until: filter.until.map(Timestamp::from),
            coverage_authority: if filter.limit.is_some() {
                CoverageAuthority::Poisoned(CoveragePoison::LimitedRequest)
            } else {
                CoverageAuthority::Eligible
            },
        };
        self.inflight
            .entry(sub_id.clone())
            .or_default()
            .push_back(snapshot);
        self.sub_id_by_wire.insert(
            (session.clone(), wire_sub_id_string(sub_id)),
            sub_id.clone(),
        );
        send_id
    }

    /// Monotonically poison every request that could have emitted an EVENT
    /// under this exact physical session and wire subscription FIFO.
    ///
    /// An overwritten NIP-01 subscription id cannot identify which outstanding
    /// revision emitted the EVENT, so the exact honest target is the union of
    /// the failure targets already present in this FIFO. A later send recorded
    /// after this call is untouched.
    pub(crate) fn poison_event_commit_failure(
        &mut self,
        session: &RelaySessionKey,
        wire_sub_id: &str,
    ) {
        let Some(sub_id) = self
            .sub_id_by_wire
            .get(&(session.clone(), wire_sub_id.to_string()))
        else {
            return;
        };
        let Some(fifo) = self.inflight.get(sub_id) else {
            return;
        };
        let targets: BTreeSet<_> = fifo
            .iter()
            .map(|snapshot| snapshot.event_failure_target)
            .collect();
        for snapshots in self.inflight.values_mut() {
            for snapshot in snapshots {
                if targets.contains(&snapshot.send_id) {
                    snapshot
                        .coverage_authority
                        .poison(CoveragePoison::EventCommitFailed);
                }
            }
        }
    }

    /// Resolve a wire subscription-id string back to the `SubId`
    /// `record_send` registered it under, if any (the same map
    /// `attribute_eose` itself reads at EOSE time). Exposed so `EngineCore`
    /// can route an inbound `NEG-MSG`/`NEG-ERR` (which only ever carries the
    /// wire string, never a `SubId`) back to the right in-flight negentropy
    /// session -- the identical lookup `attribute_eose` performs internally
    /// for EOSE, reused rather than re-implemented (plan §6 E).
    pub(crate) fn sub_id_for_wire(
        &self,
        session: &RelaySessionKey,
        wire_sub_id: &str,
    ) -> Option<SubId> {
        self.sub_id_by_wire
            .get(&(session.clone(), wire_sub_id.to_string()))
            .cloned()
    }

    /// Disconnect / pool generation bump (ruling §2 fail-safe): clear every
    /// outstanding snapshot and wire-id mapping for `session`. A replayed sub
    /// on the new generation calls [`Self::record_send`] again and gets a
    /// fresh snapshot; the pool translator (transport, C) guarantees a
    /// stale-generation frame never reaches `EngineCore` at all, so this is
    /// the only clearing path attribution needs. Scoped to the EXACT session:
    /// dropping the URL's other access contexts' snapshots here would erase
    /// coverage FIFOs for physical connections that never dropped.
    pub(crate) fn clear_session(&mut self, session: &RelaySessionKey) {
        let stale: BTreeSet<SubId> = self
            .sub_id_by_wire
            .iter()
            .filter_map(|((key, _), sub_id)| (key == session).then_some(sub_id.clone()))
            .collect();
        self.inflight.retain(|sub_id, _| !stale.contains(sub_id));
        self.sub_id_by_wire.retain(|(key, _), _| key != session);
    }

    /// Discard every outstanding snapshot and wire lookup for one exact
    /// internal subscription id. NIP-77 uses this when a reconciliation or
    /// temporary backlog request is superseded/abandoned before it can earn
    /// coverage. These ids are role-derived and never shared with the live
    /// REQ, so removing the whole FIFO is exact rather than a best-effort
    /// "pop the newest" convention.
    ///
    /// Dropping the wire mapping outright is only SAFE because no later
    /// incarnation can re-register the same string (#932): NIP-77 role ids
    /// carry an engine-minted reincarnation (`core::nip77_role_sub_id`) and
    /// planned ids are allocated tokens the router never recycles within a
    /// session (`nmp_router::SubId::allocate`). Were a discarded string ever
    /// re-registered, the FRESH FIFO underneath it would be popped by a
    /// straggler EOSE belonging to the request that was closed -- crediting
    /// durable coverage for a request the relay has not finished serving.
    pub(crate) fn discard_sub(&mut self, sub_id: &SubId) {
        self.inflight.remove(sub_id);
        self.sub_id_by_wire
            .retain(|_, mapped_sub_id| mapped_sub_id != sub_id);
    }

    /// Attribute an EOSE arriving on `session` for wire subscription id
    /// `wire_sub_id` at engine clock `eose_time`. Returns one
    /// `(CoverageKey, CoverageInterval)` pair per attributed atom — empty
    /// if the sub is unknown, its FIFO is empty (fail-safe: never
    /// reconstruct from the current plan), or every outstanding snapshot on
    /// it is `limited` (poisoned: record nothing for ANY key this EOSE
    /// might otherwise have proven).
    ///
    /// THE load-bearing rule (ruling §2): attribution is the INTERSECTION
    /// of every snapshot currently outstanding on this sub — never just the
    /// newest — because an overwriting REQ reuses the same `SubId` (M2
    /// `plan.rs`) and a relay may EOSE an in-flight straggler for an OLDER
    /// REQ after a newer one has already been sent. Crediting only the
    /// current snapshot would attribute atoms the actual terminating REQ
    /// never asked for. The oldest snapshot is popped unconditionally
    /// afterward (one REQ, one EOSE, FIFO order) — whether or not this call
    /// recorded anything.
    #[cfg(test)]
    pub(crate) fn attribute_eose(
        &mut self,
        session: &RelaySessionKey,
        wire_sub_id: &str,
        eose_time: Timestamp,
    ) -> Vec<(CoverageKey, CoverageInterval)> {
        self.attribute_eose_detailed(session, wire_sub_id, eose_time)
            .and_then(|completed| completed.eligible_claims().map(|claims| claims.to_vec()))
            .unwrap_or_default()
    }

    pub(crate) fn attribute_eose_detailed(
        &mut self,
        session: &RelaySessionKey,
        wire_sub_id: &str,
        eose_time: Timestamp,
    ) -> Option<CompletedAttribution> {
        let sub_id = self
            .sub_id_by_wire
            .get(&(session.clone(), wire_sub_id.to_string()))
            .cloned()?;
        let fifo = self.inflight.get_mut(&sub_id)?;
        if fifo.is_empty() {
            return None;
        }

        let coverage_authority = fifo
            .iter()
            .find_map(|snapshot| match snapshot.coverage_authority {
                CoverageAuthority::Eligible => None,
                poisoned @ CoverageAuthority::Poisoned(_) => Some(poisoned),
            })
            .unwrap_or(CoverageAuthority::Eligible);
        let mut result = Vec::new();
        let mut attributed: Option<BTreeSet<CoverageKey>> = None;
        let mut max_floor = Timestamp::from(0u64);
        let mut min_until = eose_time;
        for snap in fifo.iter() {
            attributed = Some(match attributed {
                None => snap.absorbed.clone(),
                Some(acc) => acc.intersection(&snap.absorbed).cloned().collect(),
            });
            if let Some(f) = snap.floor {
                max_floor = max_floor.max(f);
            }
            if let Some(u) = snap.until {
                min_until = min_until.min(u);
            }
        }
        let through = eose_time.min(min_until);
        let interval = CoverageInterval::new(max_floor, through);
        if let Some(keys) = attributed {
            result.extend(keys.into_iter().map(|k| (k, interval)));
        }

        let completed = fifo
            .pop_front()
            .expect("non-empty attribution FIFO checked above");
        Some(CompletedAttribution {
            send_id: completed.send_id,
            coverage_authority,
            claims: result,
        })
    }

    /// Attribute a completion that is structurally correlated to one exact
    /// send. This is the NEG counterpart to [`Self::attribute_eose`]: unlike
    /// an overwritten REQ's ambiguous EOSE, a live `NegSession` retains the
    /// identity returned by [`Self::record_send`], so later live-tail sends
    /// sharing its wire subscription id must not narrow the completed NEG's
    /// coverage window. `completion_time` is captured when NEG finishes,
    /// even when credit is deferred until a backfill EOSE proves ingestion.
    pub(crate) fn attribute_correlated_completion(
        &mut self,
        session: &RelaySessionKey,
        wire_sub_id: &str,
        send_id: AttributionSendId,
        completion_time: Timestamp,
    ) -> Option<CompletedAttribution> {
        let sub_id = self
            .sub_id_by_wire
            .get(&(session.clone(), wire_sub_id.to_string()))
            .cloned()?;
        let fifo = self.inflight.get_mut(&sub_id)?;
        let position = fifo
            .iter()
            .position(|snapshot| snapshot.send_id == send_id)?;
        let snapshot = fifo
            .remove(position)
            .expect("position came from this exact attribution FIFO");

        let from = snapshot.floor.unwrap_or_else(|| Timestamp::from(0u64));
        let through = snapshot
            .until
            .map_or(completion_time, |until| completion_time.min(until));
        let interval = CoverageInterval::new(from, through);
        Some(CompletedAttribution {
            send_id: snapshot.send_id,
            coverage_authority: snapshot.coverage_authority,
            claims: snapshot
                .absorbed
                .into_iter()
                .map(|key| (key, interval))
                .collect(),
        })
    }
}

/// The wire-format `subscription_id` string `EngineCore` sends a REQ under
/// for `sub_id`: the hex `Display` of its `SubId.1` digest — 64 lowercase hex
/// characters, exactly NIP-01's `subscription_id` cap, never prefixed and
/// never truncated. This is an internal implementation detail EngineCore owns
/// end-to-end (recorded at send time in `sub_id_by_wire`, read back at EOSE
/// time from the same map) — nothing else in the M3 crate graph has committed
/// to a different convention, so no other component's contract depends on
/// this exact format.
///
/// Since #899 a PLANNED sub's digest is an allocated opaque token, not a hash
/// of its filter (`nmp-router`'s `SubId::allocate`), so nothing here — or
/// anywhere else — may re-derive a wire id from a filter. The map is the only
/// authority in both directions, which it already was.
pub(crate) fn wire_sub_id_string(sub_id: &SubId) -> String {
    sub_id.1.to_string()
}
