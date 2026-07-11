//! [`MemoryStore`] — the in-memory `EventStore`, and the test ORACLE that
//! `RedbStore` is diffed against for every shared contract test
//! (`nmp-store/tests/store_contract.rs`).

use std::collections::HashMap;

use nmp_grammar::ConcreteFilter;
use nostr::filter::MatchEventOptions;
use nostr::{Event, EventId, Filter, RelayUrl};

use crate::address_key::{address_key_for, candidate_wins, AddressKey};
use crate::coverage::{
    coverage_key, merge_interval, shape_matches, shrink_after_eviction, window_erase,
};
use crate::{
    ClaimSet, CoverageInterval, CoverageKey, EventStore, GcReport, InsertOutcome, Provenance,
    RelayObserved, StoredEvent,
};

/// One coverage row as retained in memory: the window-erased shape it was
/// recorded against (needed so `gc` can test "does an evicted event match
/// this row" — see `crate::coverage::ShapeRecord`'s doc comment for why the
/// shape, not just its hash, must be retained) plus the proven interval.
#[derive(Debug, Clone)]
struct CoverageRow {
    shape: ConcreteFilter,
    interval: CoverageInterval,
}

/// An in-memory `EventStore`. Holds exactly the currently-reachable events:
/// every "regular" (non-replaceable, non-addressable) event ever inserted,
/// plus the current winner (only) for every replaceable/addressable
/// address — each carrying its merged provenance — plus coverage rows keyed
/// by `(CoverageKey, RelayUrl)`. No persistence (that is `RedbStore`'s job);
/// this store is the oracle every persistent-backend test result is diffed
/// against.
#[derive(Debug, Default)]
pub struct MemoryStore {
    by_id: HashMap<EventId, StoredEvent>,
    addr_index: HashMap<AddressKey, EventId>,
    coverage: HashMap<(CoverageKey, RelayUrl), CoverageRow>,
}

impl MemoryStore {
    /// A new, empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

impl EventStore for MemoryStore {
    fn insert(&mut self, event: Event, from: RelayObserved) -> InsertOutcome {
        // Dedup-by-id FIRST: merge provenance, no index churn, before any
        // supersession logic (ledger #5).
        if let Some(existing) = self.by_id.get_mut(&event.id) {
            let grew = existing.provenance.merge_observation(&from);
            return InsertOutcome::Duplicate {
                provenance_grew: grew,
            };
        }

        let stored = StoredEvent {
            event: event.clone(),
            provenance: Provenance::first_observation(from),
        };

        match address_key_for(&event) {
            None => {
                // Regular event: no competition, always inserted.
                self.by_id.insert(event.id, stored);
                InsertOutcome::Inserted
            }
            Some(key) => match self.addr_index.get(&key).copied() {
                None => {
                    // First event ever seen at this address.
                    let id = event.id;
                    self.by_id.insert(id, stored);
                    self.addr_index.insert(key, id);
                    InsertOutcome::Inserted
                }
                Some(current_id) => {
                    let current_event = &self
                        .by_id
                        .get(&current_id)
                        .expect("addr_index must always point at a stored event")
                        .event;

                    if candidate_wins(&event, current_event) {
                        let replaced = current_id;
                        let new_id = event.id;
                        self.by_id.remove(&replaced);
                        self.by_id.insert(new_id, stored);
                        self.addr_index.insert(key, new_id);
                        InsertOutcome::Superseded { replaced }
                    } else {
                        // Older-for-existing-address: rejected, dropped.
                        InsertOutcome::Stale
                    }
                }
            },
        }
    }

    fn query(&self, filter: &Filter) -> Vec<StoredEvent> {
        // `by_id` holds exactly the current winners (regular events, plus
        // the one live event per replaceable/addressable address) — so
        // iterating it and matching is "current winners only" by
        // construction. Matching is delegated entirely to
        // `nostr::Filter::match_event`; no hand-rolled matching here.
        self.by_id
            .values()
            .filter(|se| filter.match_event(&se.event, MatchEventOptions::new()))
            .cloned()
            .collect()
    }

    fn record_coverage(
        &mut self,
        filter: &ConcreteFilter,
        relay: &RelayUrl,
        proven: CoverageInterval,
    ) {
        let key = coverage_key(filter);
        let shape = window_erase(filter);
        let entry_key = (key, relay.clone());
        let existing = self.coverage.get(&entry_key).map(|row| row.interval);
        let merged = merge_interval(existing, proven);
        self.coverage.insert(
            entry_key,
            CoverageRow {
                shape,
                interval: merged,
            },
        );
    }

    fn get_coverage(&self, key: CoverageKey, relay: &RelayUrl) -> Option<CoverageInterval> {
        self.coverage
            .get(&(key, relay.clone()))
            .map(|row| row.interval)
    }

    fn gc(&mut self, claims: &ClaimSet) -> GcReport {
        let mut report = GcReport::default();

        // Regular events (no address key) matched by no live claim are the
        // ONLY GC candidates: replaceable/addressable current winners are
        // never in this set at all, so they are retained unconditionally,
        // by construction — never merely "protected by a check".
        let victims: Vec<EventId> = self
            .by_id
            .iter()
            .filter(|(_, se)| address_key_for(&se.event).is_none() && !claims.is_claimed(&se.event))
            .map(|(id, _)| *id)
            .collect();

        for id in victims {
            let se = self
                .by_id
                .remove(&id)
                .expect("victim id was just found in by_id");
            report.events_evicted += 1;
            let evicted_at = se.event.created_at;

            let mut to_delete = Vec::new();
            for (row_key, row) in self.coverage.iter_mut() {
                if row.interval.from <= evicted_at
                    && evicted_at <= row.interval.through
                    && shape_matches(&row.shape, &se.event)
                {
                    match shrink_after_eviction(row.interval, evicted_at) {
                        Some(shrunk) => {
                            row.interval = shrunk;
                            report.coverage_rows_shrunk += 1;
                        }
                        None => to_delete.push(row_key.clone()),
                    }
                }
            }
            for row_key in to_delete {
                self.coverage.remove(&row_key);
                report.coverage_rows_deleted += 1;
            }
        }

        report
    }
}
