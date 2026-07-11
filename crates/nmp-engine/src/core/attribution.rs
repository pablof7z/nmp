//! Coverage-attribution state (implements
//! `docs/consults/2026-07-11-fable-coverage-attribution.md` §2/§3 EXACTLY):
//! the per-`SubId` FIFO of send-time snapshots, the wire subscription-id ->
//! `SubId` reverse lookup, and the `CoverageKey` -> retained window-erased
//! shape registry `record_coverage` needs (the store only ever sees whatever
//! `ConcreteFilter` it is handed; `EngineCore` is the one place that knows
//! which shape a given key came from — see `nmp-store`'s own `ShapeRecord`
//! doc comment for the identical reasoning at the store layer).
//!
//! This is a plain data structure with no I/O and no access to the store or
//! router — `EngineCore` (`core/mod.rs`) is the one place both exist
//! together, and it is the one that actually calls `EventStore::
//! record_coverage` with the shape this module hands back.

use std::collections::{BTreeSet, HashMap, VecDeque};

use nmp_grammar::ConcreteFilter;
use nmp_router::SubId;
use nmp_store::{coverage_key, CoverageInterval, CoverageKey};
use nostr::{RelayUrl, Timestamp};

/// One send-time snapshot (ruling §2): what a single outgoing REQ (or NEG
/// session) proves, captured at the moment it was sent — never re-derived
/// from the sub's CURRENT filter later.
#[derive(Debug, Clone)]
struct AttributionSnapshot {
    absorbed: BTreeSet<CoverageKey>,
    floor: Option<Timestamp>,
    until: Option<Timestamp>,
    limited: bool,
}

/// All coverage-attribution bookkeeping `EngineCore` owns. Keyed by `SubId`
/// (which already embeds the relay — `SubId(RelayUrl, SkeletonHash)`), so a
/// FIFO lookup is also implicitly relay-scoped.
#[derive(Debug, Default)]
pub(crate) struct AttributionState {
    inflight: HashMap<SubId, VecDeque<AttributionSnapshot>>,
    /// `(relay, wire-format subscription_id string) -> SubId`, populated at
    /// send time. `nmp-transport::Pool` is an unimplemented Step 0 shell in
    /// M3 step B, so there is no pre-existing wire convention to conform
    /// to; `EngineCore` invents and owns this string entirely (see
    /// `wire_sub_id_string` below) and is the only reader of it, via this
    /// map — never by re-parsing the string back into a hash.
    sub_id_by_wire: HashMap<(RelayUrl, String), SubId>,
    /// `CoverageKey -> the window-erased ConcreteFilter shape it came from.
    /// The store's `record_coverage` takes a `&ConcreteFilter` (it computes
    /// the key AND retains the shape itself internally); `EngineCore` only
    /// ever has a `CoverageKey` at attribution time (from `WireReq::
    /// absorbed`), so it must retain the shape separately to be able to
    /// call that door at all. Grows monotonically; a stale entry for a
    /// shape no longer in demand is harmless (worst case: an unreachable
    /// key that is never looked up again).
    shape_by_key: HashMap<CoverageKey, ConcreteFilter>,
}

impl AttributionState {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Learn the window-erased shape behind every atom in `demand` (called
    /// once per recompile, from the resolver's full current `active_demand
    /// ()` — cheap, and the only way `EngineCore` ever sees the atoms'
    /// shapes at all).
    pub(crate) fn observe_demand<'a>(
        &mut self,
        demand: impl IntoIterator<Item = &'a ConcreteFilter>,
    ) {
        for atom in demand {
            self.shape_by_key
                .entry(coverage_key(atom))
                .or_insert_with(|| atom.clone());
        }
    }

    /// The retained shape for `key`, if any atom carrying it has ever been
    /// observed via [`Self::observe_demand`].
    pub(crate) fn shape_of(&self, key: CoverageKey) -> Option<ConcreteFilter> {
        self.shape_by_key.get(&key).cloned()
    }

    /// Record a send-time snapshot for a REQ just placed on the wire for
    /// `sub_id` on `relay`, whose (possibly coalesced) filter is `filter`
    /// and which absorbs `absorbed` narrow atoms (from the `WireReq` this
    /// REQ was materialized from — ruling §2's containment rule, already
    /// discharged by `nmp-router::coalesce`).
    pub(crate) fn record_send(
        &mut self,
        relay: &RelayUrl,
        sub_id: &SubId,
        filter: &ConcreteFilter,
        absorbed: BTreeSet<CoverageKey>,
    ) {
        let snapshot = AttributionSnapshot {
            absorbed,
            floor: filter.since.map(Timestamp::from),
            until: filter.until.map(Timestamp::from),
            limited: filter.limit.is_some(),
        };
        self.inflight
            .entry(sub_id.clone())
            .or_default()
            .push_back(snapshot);
        self.sub_id_by_wire
            .insert((relay.clone(), wire_sub_id_string(sub_id)), sub_id.clone());
    }

    /// Resolve a wire subscription-id string back to the `SubId`
    /// `record_send` registered it under, if any (the same map
    /// `attribute_eose` itself reads at EOSE time). Exposed so `EngineCore`
    /// can route an inbound `NEG-MSG`/`NEG-ERR` (which only ever carries the
    /// wire string, never a `SubId`) back to the right in-flight negentropy
    /// session -- the identical lookup `attribute_eose` performs internally
    /// for EOSE, reused rather than re-implemented (plan §6 E).
    pub(crate) fn sub_id_for_wire(&self, relay: &RelayUrl, wire_sub_id: &str) -> Option<SubId> {
        self.sub_id_by_wire
            .get(&(relay.clone(), wire_sub_id.to_string()))
            .cloned()
    }

    /// Disconnect / pool generation bump (ruling §2 fail-safe): clear every
    /// outstanding snapshot and wire-id mapping for `relay`. A replayed sub
    /// on the new generation calls [`Self::record_send`] again and gets a
    /// fresh snapshot; the pool translator (transport, C) guarantees a
    /// stale-generation frame never reaches `EngineCore` at all, so this is
    /// the only clearing path attribution needs.
    pub(crate) fn clear_relay(&mut self, relay: &RelayUrl) {
        self.inflight.retain(|sub_id, _| &sub_id.0 != relay);
        self.sub_id_by_wire.retain(|(url, _), _| url != relay);
    }

    /// Attribute an EOSE arriving on `relay` for wire subscription id
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
    pub(crate) fn attribute_eose(
        &mut self,
        relay: &RelayUrl,
        wire_sub_id: &str,
        eose_time: Timestamp,
    ) -> Vec<(CoverageKey, CoverageInterval)> {
        let Some(sub_id) = self
            .sub_id_by_wire
            .get(&(relay.clone(), wire_sub_id.to_string()))
            .cloned()
        else {
            return Vec::new();
        };
        let Some(fifo) = self.inflight.get_mut(&sub_id) else {
            return Vec::new();
        };
        if fifo.is_empty() {
            return Vec::new();
        }

        let poisoned = fifo.iter().any(|s| s.limited);
        let mut result = Vec::new();
        if !poisoned {
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
        }

        fifo.pop_front();
        result
    }
}

/// The wire-format `subscription_id` string `EngineCore` sends a REQ under
/// for `sub_id`: the hex `Display` of its `SkeletonHash` (already a stable,
/// canonical `nmp_grammar::DescriptorHash`). This is an internal
/// implementation detail EngineCore owns end-to-end (recorded at send time
/// in `sub_id_by_wire`, read back at EOSE time from the same map) — nothing
/// else in the M3 crate graph has committed to a different convention yet,
/// so no other component's contract depends on this exact format.
pub(crate) fn wire_sub_id_string(sub_id: &SubId) -> String {
    sub_id.1.to_string()
}
