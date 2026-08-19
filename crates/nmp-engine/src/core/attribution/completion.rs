//! Send-generation recording, retirement, and completion attribution.

use std::collections::BTreeSet;

use nmp_grammar::{ConcreteFilter, RelaySessionKey};
use nmp_router::SubId;
use nmp_store::{CoverageInterval, CoverageKey};
use nostr::Timestamp;

use super::{
    wire_sub_id_string, AttributionSendId, AttributionSnapshot, AttributionState,
    CompletedAttribution, CompletedCoverageClaim, CoverageAuthority, CoveragePoison,
};

impl AttributionState {
    /// Record a send-time snapshot for a REQ just placed on the wire for
    /// `sub_id` on `session`, whose (possibly coalesced) filter is `filter`
    /// and which absorbs `coverage_claims` narrow atoms (from the `WireReq` this
    /// REQ was materialized from — ruling §2's containment rule, already
    /// discharged by `nmp-router::coalesce`).
    pub(crate) fn record_send(
        &mut self,
        session: &RelaySessionKey,
        sub_id: &SubId,
        filter: &ConcreteFilter,
        coverage_claims: BTreeSet<CoverageKey>,
    ) -> AttributionSendId {
        let send_id = AttributionSendId(self.next_send_id);
        self.next_send_id = self.next_send_id.wrapping_add(1);
        let snapshot = AttributionSnapshot {
            send_id,
            coverage_claims,
            filter_hash: filter.hash(),
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
            .map(|snapshot| snapshot.send_id)
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
    /// `attribute_eose` itself reads at EOSE time).
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
    /// stale-generation frame never reaches `CoreState` at all, so this is
    /// the only clearing path attribution needs. Scoped to the EXACT session:
    /// dropping the URL's other access contexts' snapshots here would erase
    /// coverage FIFOs for physical connections that never dropped.
    pub(crate) fn clear_session(&mut self, session: &RelaySessionKey) {
        let stale: BTreeSet<SubId> = self
            .sub_id_by_wire
            .iter()
            .filter_map(|((key, _), sub_id)| (key == session).then_some(sub_id.clone()))
            .collect();
        for sub_id in &stale {
            self.inflight.remove(sub_id);
        }
        self.sub_id_by_wire.retain(|(key, _), _| key != session);
    }

    /// Discard every outstanding snapshot and wire lookup for one exact
    /// internal subscription id, used when a request is superseded or
    /// abandoned before it can earn coverage. Removing the whole FIFO is
    /// exact rather than a best-effort
    /// "pop the newest" convention.
    ///
    /// Dropping the wire mapping outright is only SAFE because no later
    /// incarnation can re-register the same string (#932):
    /// planned ids are allocated tokens the router never recycles within a
    /// session (`nmp_router::SubId::allocate`). Were a discarded string ever
    /// re-registered, the FRESH FIFO underneath it would be popped by a
    /// straggler EOSE belonging to the request that was closed -- crediting
    /// durable coverage for a request the relay has not finished serving.
    pub(crate) fn discard_sub(&mut self, sub_id: &SubId) {
        self.inflight.remove(sub_id);
        let session = RelaySessionKey::new(sub_id.0.clone(), sub_id.2);
        self.sub_id_by_wire
            .remove(&(session, wire_sub_id_string(sub_id)));
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
                None => snap.coverage_claims.clone(),
                Some(acc) => acc.intersection(&snap.coverage_claims).cloned().collect(),
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
            .expect("an in-flight FIFO is removed the moment it empties");
        let drained = fifo.is_empty();
        if drained {
            self.inflight.remove(&sub_id);
        }
        Some(complete_snapshot(sub_id, completed, coverage_authority, result))
    }
}

/// Turn the intersected claim set into the completion the persistence door
/// takes. Free-standing rather than a method because it reads no attribution
/// state: a `CoverageKey` IS the coverage identity `record_coverage` wants,
/// so there is nothing left to look up.
fn complete_snapshot(
    sub_id: SubId,
    snapshot: AttributionSnapshot,
    coverage_authority: CoverageAuthority,
    claims: Vec<(CoverageKey, CoverageInterval)>,
) -> CompletedAttribution {
    CompletedAttribution {
        sub_id,
        send_id: snapshot.send_id,
        filter_hash: snapshot.filter_hash,
        coverage_authority,
        claims: claims
            .into_iter()
            .map(|(key, interval)| CompletedCoverageClaim { key, interval })
            .collect(),
    }
}
