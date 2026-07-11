//! [`MemoryStore`] — the in-memory `EventStore`, and the test ORACLE that
//! `RedbStore` is diffed against for every shared contract test
//! (`nmp-store/tests/store_contract.rs`).

use std::collections::{BTreeMap, HashMap, HashSet};

use nmp_grammar::ConcreteFilter;
use nostr::filter::MatchEventOptions;
use nostr::secp256k1::schnorr::Signature;
use nostr::{Event, EventId, Filter, Kind, PublicKey, RelayUrl, Timestamp};

use crate::address_key::{address_key_for, address_key_for_coordinate, candidate_wins, AddressKey};
use crate::coverage::{
    coverage_key, merge_interval, shape_matches, shrink_after_eviction, window_erase,
};
use crate::{
    AcceptOutcome, AcceptWrite, ClaimSet, CompensateOutcome, CoverageInterval, CoverageKey,
    EventStore, GcReport, InsertOutcome, IntentId, IntentSigState, LocalOrigin, PromoteOutcome,
    Provenance, ReceiptState, RecoveredIntent, RecoveredReceipt, RefuseReason, RelayObserved,
    RetractReason, SigState, StoredEvent, WriteDurability,
};

/// One `OUTBOX_INTENTS` row (M3 durable-outbox unit, crashsafe-accepted-2-3-
/// plan.md §2.2) as retained in memory. `MemoryStore` implements the same
/// door SEMANTICS as `RedbStore` so the two backends can never diverge on
/// the outbox contract (this struct is the in-memory mirror of
/// `RedbStore`'s `OUTBOX_INTENTS` JSON record) — but carries no durability
/// guarantee of its own (Fable checkpoint Q4): `recover_outbox` always
/// returns empty, because nothing here survives a process crash by
/// construction. Its fields are therefore write-only from this backend's
/// own perspective (never read back by `MemoryStore` itself, only kept in
/// lockstep with what `accept_write`/`promote_signed` would persist on
/// `RedbStore`) — `#[allow(dead_code)]` records that deliberately, rather
/// than dropping the fields and letting the two backends' journal shapes
/// silently diverge.
#[allow(dead_code)]
#[derive(Debug, Clone)]
struct OutboxIntentRecord {
    receipt_id: u64,
    frozen: Event,
    expected_pubkey: PublicKey,
    signing_identity_ref: String,
    durability: WriteDurability,
    routing: String,
    sig_state: IntentSigState,
    accepted_at: Timestamp,
}

/// An address-tombstone's durable fact: which kind:5 event set the
/// deletion ceiling, and (diagnostics only — the ceiling comparison alone
/// decides refusal) that kind:5's own author. Retention is PERMANENT
/// (retraction-and-negative-deltas.md §7 owner decision) — never GC-claimed.
/// Id-tombstones do NOT use this: see `MemoryStore::deleted_ids`'s doc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TombstoneRecord {
    deleting_event_id: EventId,
    deleting_author: PublicKey,
}

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
    /// Permanent kind:5 tombstones for individual event ids
    /// (retraction-and-negative-deltas.md §2/§7), keyed `(target id,
    /// deleting author)` -- value is the deleting kind:5's own id
    /// (diagnostics only). NOT collapsed to one record per target id: the
    /// target's real author is unknown until it actually arrives, so an
    /// unauthorized third party can always name an id that's already been
    /// (or will be) legitimately deleted by its real author. If a single
    /// slot per id were overwritable, that unauthorized kind:5 would
    /// silently replace -- and so undo -- the real author's permanent,
    /// authorized deletion the moment the real target is redelivered. Every
    /// distinct claiming author gets its own permanent entry instead; a
    /// redelivered target is refused iff ITS OWN author is among the
    /// claimants, regardless of how many other (irrelevant) authors also
    /// named that id. Never GC-claimed.
    deleted_ids: HashMap<(EventId, PublicKey), EventId>,
    /// Permanent kind:5 tombstones for replaceable/addressable addresses:
    /// the highest deleting-event `created_at` seen for that address (the
    /// "ceiling") plus the record of the deletion that set it. A candidate
    /// with `created_at <= ceiling` is tombstoned; NIP-09 allows a fresh
    /// post-deletion event at the same address to win normally.
    deleted_addrs: HashMap<AddressKey, (Timestamp, TombstoneRecord)>,
    /// Persistent NIP-40 expiration index: `expiry_ts -> {ids expiring at
    /// that instant}`, kept in lockstep with `by_id` on every insert and
    /// every removal so `expire_due`/`next_expiration` never rescan the
    /// whole store (retraction-and-negative-deltas.md §3.1).
    expiration_index: BTreeMap<Timestamp, HashSet<EventId>>,
    /// `OUTBOX_INTENTS` mirror (crashsafe-accepted-2-3-plan.md §2.2) — one
    /// entry per still-open locally-accepted write intent.
    outbox_intents: HashMap<IntentId, OutboxIntentRecord>,
    /// `OUTBOX_DISPLACED` mirror: the predecessor each open intent
    /// evicted, if any, kept durable-in-memory until `promote_signed` or
    /// `compensate_write` drops it.
    outbox_displaced: HashMap<IntentId, StoredEvent>,
    /// `OUTBOX_META`'s in-memory mirror: the next `IntentId` to allocate.
    /// The store owns this counter (never a caller) — see `IntentId`'s doc
    /// for why a caller-inferred value is unsound.
    next_intent_id: u64,
    /// `OUTBOX_RECEIPTS` mirror: retained receipt records, independent of
    /// `outbox_intents`'s open-work rows (architecture review correction —
    /// see `ReceiptState`'s doc). Never pruned by this unit.
    outbox_receipts: HashMap<u64, RecoveredReceipt>,
}

impl MemoryStore {
    /// A new, empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocate the next `IntentId` from the store's own durable
    /// high-water mark (never inferred from the currently-open set — see
    /// `IntentId`'s doc). Starts at 1 (0 is never issued, kept free as an
    /// unambiguous "no id" sentinel for callers that want one).
    fn alloc_intent_id(&mut self) -> IntentId {
        self.next_intent_id += 1;
        IntentId(self.next_intent_id)
    }

    /// Write (or overwrite) one `OUTBOX_INTENTS` row plus its
    /// `OUTBOX_DISPLACED` stash, if any — `accept_write`'s journal half of
    /// the "one atomic commit" (in-memory: same call, no separate
    /// transaction to span).
    #[allow(clippy::too_many_arguments)]
    fn journal_intent(
        &mut self,
        intent_id: IntentId,
        receipt_id: u64,
        frozen: Event,
        expected_pubkey: PublicKey,
        signing_identity_ref: String,
        durability: WriteDurability,
        routing: String,
        sig_state: IntentSigState,
        accepted_at: Timestamp,
        displaced: Option<StoredEvent>,
    ) {
        self.outbox_intents.insert(
            intent_id,
            OutboxIntentRecord {
                receipt_id,
                frozen,
                expected_pubkey,
                signing_identity_ref,
                durability,
                routing,
                sig_state,
                accepted_at,
            },
        );
        if let Some(displaced) = displaced {
            self.outbox_displaced.insert(intent_id, displaced);
        }
    }

    /// Write one `OUTBOX_RECEIPTS` row at `Accepted` — `accept_write`'s
    /// receipt-retention half (architecture review correction). Always
    /// paired with `journal_intent` in the same call; never pruned by this
    /// unit.
    fn journal_receipt(
        &mut self,
        receipt_id: u64,
        intent_id: IntentId,
        frozen_id: EventId,
        expected_pubkey: PublicKey,
    ) {
        self.outbox_receipts.insert(
            receipt_id,
            RecoveredReceipt {
                receipt_id,
                intent_id: Some(intent_id),
                frozen_id,
                expected_pubkey,
                state: ReceiptState::Accepted,
            },
        );
    }

    /// Re-admit a durably-stashed predecessor `se` through the ordinary
    /// dedup/tombstone/supersession rules `insert` runs, preserving its
    /// FULL original provenance (both relay `seen` history and any `local`
    /// origin) rather than reconstructing it from a single fresh
    /// observation — the compensating re-insert retraction-and-negative-
    /// deltas.md §4.2 describes ("through the same one door... wins its
    /// address back by ordinary supersession rules"), never an
    /// un-supersede operation. Returns the row as it now stands if `se`
    /// actually (re)claims a slot; `None` if it is refused, deduped away,
    /// or loses the address race (`Stale` — the correct, silent §3.4
    /// outcome for a re-offered grand-predecessor: nothing churns).
    fn reinsert_stashed(&mut self, se: StoredEvent) -> Option<StoredEvent> {
        let event = se.event.clone();

        if let Some(existing) = self.by_id.get_mut(&event.id) {
            for (relay, at) in &se.provenance.seen {
                existing
                    .provenance
                    .merge_observation(&RelayObserved::new(relay.clone(), *at));
            }
            return Some(existing.clone());
        }
        if self.tombstone_refuses(&event) {
            return None;
        }

        match address_key_for(&event) {
            None => {
                self.index_expiration(&se);
                self.by_id.insert(event.id, se.clone());
                Some(se)
            }
            Some(key) => match self.addr_index.get(&key).copied() {
                None => {
                    self.index_expiration(&se);
                    self.by_id.insert(event.id, se.clone());
                    self.addr_index.insert(key, event.id);
                    Some(se)
                }
                Some(current_id) => {
                    let current_event = &self
                        .by_id
                        .get(&current_id)
                        .expect("addr_index must always point at a stored event")
                        .event;
                    if candidate_wins(&event, current_event) {
                        let replaced = self
                            .by_id
                            .remove(&current_id)
                            .expect("addr_index must always point at a stored event");
                        self.unindex_expiration(&replaced);
                        self.index_expiration(&se);
                        self.by_id.insert(event.id, se.clone());
                        self.addr_index.insert(key, event.id);
                        Some(se)
                    } else {
                        None
                    }
                }
            },
        }
    }

    /// Add `se` to the expiration index if it carries a NIP-40 `expiration`
    /// tag. Called for every row entering `by_id`.
    fn index_expiration(&mut self, se: &StoredEvent) {
        if let Some(ts) = se.event.tags.expiration().copied() {
            self.expiration_index
                .entry(ts)
                .or_default()
                .insert(se.event.id);
        }
    }

    /// Remove `se` from the expiration index, if it was in it. Called for
    /// every row leaving `by_id` (supersession's evicted row, `remove`).
    fn unindex_expiration(&mut self, se: &StoredEvent) {
        if let Some(ts) = se.event.tags.expiration().copied() {
            if let Some(ids) = self.expiration_index.get_mut(&ts) {
                ids.remove(&se.event.id);
                if ids.is_empty() {
                    self.expiration_index.remove(&ts);
                }
            }
        }
    }

    /// The tombstone check (retraction-and-negative-deltas.md §2): `true`
    /// iff `event` must be `Refused(Tombstoned)`. Runs for every event, not
    /// just kind:5 redeliveries — a kind:5 event's own id could itself have
    /// been the target of an earlier (unusual but not forbidden) deletion.
    ///
    /// For an id-tombstone, this is where the deferred NIP-09 author-only
    /// check happens for a target that was NOT held at deletion time (the
    /// `deleted_ids` entry was written speculatively, before this event
    /// ever arrived): refused iff `event.pubkey` itself is among the
    /// authors who have claimed this id (`deleted_ids` is keyed per-author,
    /// not collapsed to one slot -- see its doc for why). A wrong-author
    /// claim on this same id never suppresses this event: it simply isn't
    /// in the set.
    fn tombstone_refuses(&self, event: &Event) -> bool {
        if self.deleted_ids.contains_key(&(event.id, event.pubkey)) {
            return true;
        }
        if let Some(key) = address_key_for(event) {
            if let Some((ceiling, _)) = self.deleted_addrs.get(&key) {
                if event.created_at <= *ceiling {
                    return true;
                }
            }
        }
        false
    }

    /// Kind:5 processing (retraction-and-negative-deltas.md §2), run once
    /// the deleting event itself has been durably stored. For each `e`-tag
    /// id / `a`-tag coordinate: author-verify (immediately if the target is
    /// held or the coordinate carries its own pubkey; deferred via
    /// `tombstone_refuses` otherwise), write the PERMANENT tombstone, and
    /// drop the row if currently held. Returns every row actually dropped.
    fn process_kind5_deletions(&mut self, deleting: &Event) -> Vec<StoredEvent> {
        let mut deleted = Vec::new();

        let target_ids: Vec<EventId> = deleting.tags.event_ids().copied().collect();
        for target_id in target_ids {
            let authorized_and_held = self
                .by_id
                .get(&target_id)
                .is_some_and(|se| se.event.pubkey == deleting.pubkey);
            if authorized_and_held {
                if let Some(removed) = self.remove(target_id, RetractReason::Deleted) {
                    deleted.push(removed);
                }
            }
            // Claim recorded regardless of hold state right now -- a
            // target not yet held is checked, deferred, by
            // `tombstone_refuses` at the moment it actually arrives. NEVER
            // overwrite another author's existing claim on this same id
            // (see `deleted_ids`'s doc) -- accumulate.
            self.deleted_ids
                .insert((target_id, deleting.pubkey), deleting.id);
        }

        let coords: Vec<_> = deleting.tags.coordinates().cloned().collect();
        for coord in coords {
            if coord.public_key != deleting.pubkey {
                // NIP-09 author-only: a coordinate naming a pubkey other
                // than this deletion's own author carries no authority at
                // all here -- skip entirely, no tombstone recorded.
                continue;
            }
            let Some(key) = address_key_for_coordinate(&coord) else {
                continue;
            };

            let record = TombstoneRecord {
                deleting_event_id: deleting.id,
                deleting_author: deleting.pubkey,
            };
            let raises_ceiling = self
                .deleted_addrs
                .get(&key)
                .is_none_or(|(ceiling, _)| deleting.created_at > *ceiling);
            if raises_ceiling {
                self.deleted_addrs
                    .insert(key.clone(), (deleting.created_at, record));
            }

            if let Some(current_id) = self.addr_index.get(&key).copied() {
                let held_at_or_before = self
                    .by_id
                    .get(&current_id)
                    .is_some_and(|se| se.event.created_at <= deleting.created_at);
                if held_at_or_before {
                    if let Some(removed) = self.remove(current_id, RetractReason::Deleted) {
                        deleted.push(removed);
                    }
                }
            }
        }

        deleted
    }
}

/// True iff `se` is a locally-authored row still awaiting a signature —
/// the GC-exclusion predicate (Fable checkpoint R5), shared by `gc`'s
/// candidacy filter.
fn is_open_local_intent(se: &StoredEvent) -> bool {
    matches!(
        se.provenance.local,
        Some(LocalOrigin {
            sig_state: SigState::Pending,
            ..
        })
    )
}

impl EventStore for MemoryStore {
    fn insert(&mut self, event: Event, from: RelayObserved) -> InsertOutcome {
        // Refused at the door FIRST: an already-expired event is never
        // stored, so it never touches dedup or supersession at all.
        if event.is_expired_at(&from.at) {
            return InsertOutcome::Refused(RefuseReason::AlreadyExpired);
        }

        // Dedup-by-id FIRST: merge provenance, no index churn, before any
        // tombstone or supersession logic (ledger #5).
        if let Some(existing) = self.by_id.get_mut(&event.id) {
            let grew = existing.provenance.merge_observation(&from);
            return InsertOutcome::Duplicate {
                provenance_grew: grew,
            };
        }

        // Tombstone check, AFTER dedup-by-id, BEFORE storage
        // (retraction-and-negative-deltas.md §2).
        if self.tombstone_refuses(&event) {
            return InsertOutcome::Refused(RefuseReason::Tombstoned);
        }

        let is_deletion = event.kind == Kind::EventDeletion;
        let stored = StoredEvent {
            event: event.clone(),
            provenance: Provenance::first_observation(from),
        };

        let outcome = match address_key_for(&event) {
            None => {
                // Regular event: no competition, always inserted.
                self.index_expiration(&stored);
                self.by_id.insert(event.id, stored);
                InsertOutcome::Inserted
            }
            Some(key) => match self.addr_index.get(&key).copied() {
                None => {
                    // First event ever seen at this address.
                    let id = event.id;
                    self.index_expiration(&stored);
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
                        let new_id = event.id;
                        let replaced = self
                            .by_id
                            .remove(&current_id)
                            .expect("addr_index must always point at a stored event");
                        self.unindex_expiration(&replaced);
                        self.index_expiration(&stored);
                        self.by_id.insert(new_id, stored);
                        self.addr_index.insert(key, new_id);
                        InsertOutcome::Superseded {
                            replaced: Box::new(replaced),
                        }
                    } else {
                        // Older-for-existing-address: rejected, dropped.
                        InsertOutcome::Stale
                    }
                }
            },
        };

        // Kind:5 has no replaceable/addressable address (M1's set excludes
        // it), so `outcome` above is always `Inserted` here, by
        // construction -- process its deletions now that the event itself
        // is durably stored (re-servable, per §2).
        if is_deletion {
            if let InsertOutcome::Inserted = outcome {
                let deleted = self.process_kind5_deletions(&event);
                return InsertOutcome::Kind5Processed { deleted };
            }
        }

        outcome
    }

    fn remove(&mut self, id: EventId, _reason: RetractReason) -> Option<StoredEvent> {
        let removed = self.by_id.remove(&id)?;
        // Clear the address index too, but ONLY if it still points at the
        // row we just removed — `id` may be a non-addressed regular event
        // (no entry to clear), or a stale/superseded id that never held the
        // address slot in the first place.
        if let Some(key) = address_key_for(&removed.event) {
            if self.addr_index.get(&key) == Some(&id) {
                self.addr_index.remove(&key);
            }
        }
        self.unindex_expiration(&removed);
        Some(removed)
    }

    fn expire_due(&mut self, now: Timestamp) -> Vec<StoredEvent> {
        let due: Vec<EventId> = self
            .expiration_index
            .range(..=now)
            .flat_map(|(_, ids)| ids.iter().copied())
            .collect();

        due.into_iter()
            .filter_map(|id| self.remove(id, RetractReason::Expired))
            .collect()
    }

    fn next_expiration(&self) -> Option<Timestamp> {
        self.expiration_index.keys().next().copied()
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

        // Regular events (no address key) matched by no live claim, AND not
        // an open (unsigned) local intent, are the ONLY GC candidates:
        // replaceable/addressable current winners are never in this set at
        // all (retained unconditionally, by construction), and neither is
        // an unsigned pending row (Fable checkpoint R5 — an open intent
        // must never be evicted before it ever signs; once
        // `promote_signed` flips it to `Signed` it becomes an ordinary
        // event again, GC-able like any other under `claims`).
        let victims: Vec<EventId> = self
            .by_id
            .iter()
            .filter(|(_, se)| {
                address_key_for(&se.event).is_none()
                    && !is_open_local_intent(se)
                    && !claims.is_claimed(&se.event)
            })
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

    fn accept_write(&mut self, accept: AcceptWrite) -> AcceptOutcome {
        let AcceptWrite {
            receipt_id,
            frozen,
            expected_pubkey,
            signing_identity_ref,
            durability,
            routing,
            sig_state,
            accepted_at,
        } = accept;

        // Refused at the door FIRST, same as `insert`: never journaled,
        // nothing to recover, and (R7 correction) no `IntentId` is ever
        // allocated for a refused call — a refusal can never burn an id.
        if frozen.is_expired_at(&accepted_at) {
            return AcceptOutcome::Refused(RefuseReason::AlreadyExpired);
        }
        if self.tombstone_refuses(&frozen) {
            return AcceptOutcome::Refused(RefuseReason::Tombstoned);
        }

        let intent_id = self.alloc_intent_id();
        let local = LocalOrigin {
            intent_id,
            sig_state: SigState::Pending,
            accepted_at,
        };

        // Dedup-by-id: an edge case (a fresh intent's frozen id colliding
        // with an already-held row), NOT the ordinary relay-echo hand-off
        // (that always arrives through `insert`, after this row's real
        // signature already replaced the sentinel — see `promote_signed`'s
        // doc). The intent is still journaled: it still gets signed and
        // delivered even though it does not (re)claim the row here.
        if let Some(existing) = self.by_id.get(&frozen.id) {
            let row = existing.clone();
            let frozen_id = frozen.id;
            self.journal_intent(
                intent_id,
                receipt_id,
                frozen,
                expected_pubkey,
                signing_identity_ref,
                durability,
                routing,
                sig_state,
                accepted_at,
                None,
            );
            self.journal_receipt(receipt_id, intent_id, frozen_id, expected_pubkey);
            return AcceptOutcome::Duplicate { intent_id, row };
        }

        let stored = StoredEvent {
            event: frozen.clone(),
            provenance: Provenance::local_origin(local),
        };

        let (outcome, displaced) = match address_key_for(&stored.event) {
            None => {
                self.index_expiration(&stored);
                self.by_id.insert(stored.event.id, stored.clone());
                (
                    AcceptOutcome::Inserted {
                        intent_id,
                        row: stored,
                    },
                    None,
                )
            }
            Some(key) => match self.addr_index.get(&key).copied() {
                None => {
                    let id = stored.event.id;
                    self.index_expiration(&stored);
                    self.by_id.insert(id, stored.clone());
                    self.addr_index.insert(key, id);
                    (
                        AcceptOutcome::Inserted {
                            intent_id,
                            row: stored,
                        },
                        None,
                    )
                }
                Some(current_id) => {
                    let current_event = &self
                        .by_id
                        .get(&current_id)
                        .expect("addr_index must always point at a stored event")
                        .event;

                    if candidate_wins(&stored.event, current_event) {
                        let new_id = stored.event.id;
                        let replaced = self
                            .by_id
                            .remove(&current_id)
                            .expect("addr_index must always point at a stored event");
                        self.unindex_expiration(&replaced);
                        self.index_expiration(&stored);
                        self.by_id.insert(new_id, stored.clone());
                        self.addr_index.insert(key, new_id);
                        (
                            AcceptOutcome::Superseded {
                                intent_id,
                                row: stored,
                                replaced: Box::new(replaced.clone()),
                            },
                            Some(replaced),
                        )
                    } else {
                        (AcceptOutcome::Stale { intent_id }, None)
                    }
                }
            },
        };

        let frozen_id = frozen.id;
        self.journal_intent(
            intent_id,
            receipt_id,
            frozen,
            expected_pubkey,
            signing_identity_ref,
            durability,
            routing,
            sig_state,
            accepted_at,
            displaced,
        );
        self.journal_receipt(receipt_id, intent_id, frozen_id, expected_pubkey);

        outcome
    }

    fn promote_signed(&mut self, id: EventId, sig: Signature) -> PromoteOutcome {
        let Some(se) = self.by_id.get_mut(&id) else {
            return PromoteOutcome::NotFound;
        };
        let Some(local) = se.provenance.local.as_mut() else {
            return PromoteOutcome::NotFound;
        };
        let intent_id = local.intent_id;
        local.sig_state = SigState::Signed;
        se.event.sig = sig;
        let row = se.clone();

        // Same transaction (in-memory: same call), per R6 — durably drop
        // the displaced stash so recovery after a promote never sees it.
        self.outbox_displaced.remove(&intent_id);
        if let Some(record) = self.outbox_intents.get_mut(&intent_id) {
            record.sig_state = IntentSigState::Signed;
            record.frozen = row.event.clone();
        }
        // Architecture review correction: the RETAINED receipt record
        // (separate from `outbox_intents`'s open-work row) tracks the same
        // transition — `reattach_receipt` must reflect `Signed` from here
        // on, independent of whether the open-work row survives.
        if let Some(receipt) = self
            .outbox_receipts
            .values_mut()
            .find(|r| r.intent_id == Some(intent_id))
        {
            receipt.state = ReceiptState::Signed;
        }

        PromoteOutcome::Promoted { row: Box::new(row) }
    }

    fn compensate_write(&mut self, id: EventId) -> CompensateOutcome {
        let Some(intent_id) = self
            .by_id
            .get(&id)
            .and_then(|se| se.provenance.local.as_ref())
            .filter(|local| local.sig_state == SigState::Pending)
            .map(|local| local.intent_id)
        else {
            return CompensateOutcome::NotFound;
        };

        // §4.2: `remove(id, Rejected)` writes no tombstone (`remove` never
        // writes one — only kind:5 processing does), then re-insert the
        // stashed predecessor through the SAME one door — ordinary
        // supersession, never an un-supersede operation.
        self.remove(id, RetractReason::Rejected);
        self.outbox_intents.remove(&intent_id);
        let restored = self
            .outbox_displaced
            .remove(&intent_id)
            .and_then(|displaced| self.reinsert_stashed(displaced))
            .map(Box::new);

        // Architecture review correction: `OUTBOX_INTENTS`'s open-work row
        // is gone (line above), but the RETAINED receipt record survives —
        // `reattach_receipt` must still answer `Compensated`, not `None`.
        if let Some(receipt) = self
            .outbox_receipts
            .values_mut()
            .find(|r| r.intent_id == Some(intent_id))
        {
            receipt.state = ReceiptState::Compensated;
        }

        CompensateOutcome::Compensated { restored }
    }

    fn recover_outbox(&self) -> Vec<RecoveredIntent> {
        // Fable checkpoint Q4: crash-safety is a `RedbStore`-only backend
        // property. Nothing here survives a real process crash, so there
        // is nothing to recover, by construction.
        Vec::new()
    }

    fn reattach_receipt(&self, receipt_id: u64) -> Option<RecoveredReceipt> {
        // NOT a Q4 "always empty" door: retention (not crash-survival) is
        // the contract here, and `MemoryStore` retains faithfully for the
        // life of the process — see `EventStore::reattach_receipt`'s doc.
        self.outbox_receipts.get(&receipt_id).cloned()
    }

    fn accept_ephemeral(
        &mut self,
        receipt_id: u64,
        frozen_id: EventId,
        expected_pubkey: PublicKey,
    ) {
        // Receipt-ONLY: no EVENTS row, no OUTBOX_INTENTS row — nothing
        // backs `intent_id` at all (`None`). `MemoryStore` never models a
        // real crash (Q4), so there is no boot-time reconciliation to
        // `Abandoned` here — an ephemeral receipt just stays `Accepted`
        // for the life of the process unless the engine transitions it
        // itself (out of this unit's scope).
        self.outbox_receipts.insert(
            receipt_id,
            RecoveredReceipt {
                receipt_id,
                intent_id: None,
                frozen_id,
                expected_pubkey,
                state: ReceiptState::Accepted,
            },
        );
    }
}
