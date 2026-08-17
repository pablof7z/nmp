//! Send-generation recording, retirement, and completion attribution.

use std::collections::BTreeSet;
#[cfg(test)]
use std::collections::VecDeque;

use nmp_grammar::{ConcreteFilter, RelaySessionKey};
use nmp_router::SubId;
use nmp_store::{CoverageInterval, CoverageKey};
use nostr::Timestamp;

use super::{
    wire_sub_id_string, AttributionSendId, AttributionSnapshot, AttributionState,
    CompletedAttribution, CompletedCoverageClaim, CoverageAuthority, CoveragePoison,
    EventFailureTarget,
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
        event_failure_target: EventFailureTarget,
    ) -> AttributionSendId {
        let send_id = AttributionSendId(self.next_send_id);
        self.next_send_id = self.next_send_id.wrapping_add(1);
        for key in &coverage_claims {
            *self.inflight_shape_owner_counts.entry(*key).or_insert(0) += 1;
        }
        let snapshot = AttributionSnapshot {
            send_id,
            event_failure_target: match event_failure_target {
                EventFailureTarget::ThisSend => send_id,
                EventFailureTarget::Correlated(send_id) => send_id,
            },
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
    /// `attribute_eose` itself reads at EOSE time). Exposed so `CoreState`
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
        let removed: Vec<_> = stale
            .iter()
            .filter_map(|sub_id| self.inflight.remove(sub_id))
            .flatten()
            .collect();
        for snapshot in removed {
            self.release_snapshot(&snapshot);
        }
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
        if let Some(removed) = self.inflight.remove(sub_id) {
            for snapshot in removed {
                self.release_snapshot(&snapshot);
            }
        }
        let session = RelaySessionKey::new(sub_id.0.clone(), sub_id.2);
        self.sub_id_by_wire
            .remove(&(session, wire_sub_id_string(sub_id)));
    }

    #[cfg(test)]
    pub(crate) fn current_claims(&self, sub_id: &SubId) -> BTreeSet<CoverageKey> {
        self.inflight
            .get(sub_id)
            .and_then(VecDeque::back)
            .map(|snapshot| snapshot.coverage_claims.clone())
            .unwrap_or_default()
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
    /// of every accepted snapshot currently outstanding on this exact wire
    /// id/filter generation — never just the newest. Replay or repeated
    /// accepted delivery can leave more than one outstanding attempt for the
    /// same immutable request, and a relay may EOSE the older attempt after a
    /// newer one was sent. Crediting only the current snapshot would attribute
    /// atoms the actual terminating REQ never asked for. The oldest snapshot
    /// is popped unconditionally afterward (one REQ, one EOSE, FIFO order) —
    /// whether or not this call recorded anything.
    #[cfg(test)]
    pub(crate) fn attribute_eose(
        &mut self,
        session: &RelaySessionKey,
        wire_sub_id: &str,
        eose_time: Timestamp,
    ) -> Vec<(CoverageKey, CoverageInterval)> {
        self.attribute_eose_detailed(session, wire_sub_id, eose_time)
            .and_then(|completed| completed.eligible_claims())
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
        Some(self.complete_snapshot(sub_id, completed, coverage_authority, result))
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
        if fifo.is_empty() {
            self.inflight.remove(&sub_id);
        }
        let from = snapshot.floor.unwrap_or_else(|| Timestamp::from(0u64));
        let through = snapshot
            .until
            .map_or(completion_time, |until| completion_time.min(until));
        let interval = CoverageInterval::new(from, through);
        let coverage_authority = snapshot.coverage_authority;
        let claims = snapshot
            .coverage_claims
            .iter()
            .copied()
            .map(|key| (key, interval))
            .collect();
        Some(self.complete_snapshot(sub_id, snapshot, coverage_authority, claims))
    }

    fn complete_snapshot(
        &mut self,
        sub_id: SubId,
        snapshot: AttributionSnapshot,
        mut coverage_authority: CoverageAuthority,
        claims: Vec<(CoverageKey, CoverageInterval)>,
    ) -> CompletedAttribution {
        let completed_claims: Option<Vec<_>> = claims
            .iter()
            .map(|(key, interval)| {
                self.shape_by_key
                    .get(key)
                    .cloned()
                    .map(|atom| CompletedCoverageClaim {
                        key: *key,
                        atom,
                        interval: *interval,
                    })
            })
            .collect();
        let claims = completed_claims.unwrap_or_else(|| {
            coverage_authority.poison(CoveragePoison::MissingShape);
            Vec::new()
        });
        self.release_snapshot(&snapshot);
        CompletedAttribution {
            sub_id,
            send_id: snapshot.send_id,
            filter_hash: snapshot.filter_hash,
            coverage_authority,
            claims,
        }
    }

    pub(super) fn release_active_shape_owner(&mut self, key: CoverageKey) {
        if let Some(count) = self.active_shape_owner_counts.get_mut(&key) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.active_shape_owner_counts.remove(&key);
            }
        }
        self.remove_unowned_shape(key);
    }

    pub(super) fn release_live_shape_owner(&mut self, key: CoverageKey) {
        if let Some(count) = self.live_shape_owner_counts.get_mut(&key) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.live_shape_owner_counts.remove(&key);
            }
        }
        self.remove_unowned_shape(key);
    }

    pub(super) fn release_snapshot(&mut self, snapshot: &AttributionSnapshot) {
        for key in &snapshot.coverage_claims {
            if let Some(count) = self.inflight_shape_owner_counts.get_mut(key) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    self.inflight_shape_owner_counts.remove(key);
                }
            }
            self.remove_unowned_shape(*key);
        }
    }

    pub(super) fn remove_unowned_shape(&mut self, key: CoverageKey) {
        if !self.active_shape_owner_counts.contains_key(&key)
            && !self.live_shape_owner_counts.contains_key(&key)
            && !self.inflight_shape_owner_counts.contains_key(&key)
        {
            self.shape_by_key.remove(&key);
        }
    }
}
